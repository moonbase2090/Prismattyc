//! Frames between pmuxd and a pane holder.
//!
//! On Windows a pane's pseudoconsole and program belong to a small holder
//! process (`pmuxd --pty-holder`), so the program can outlive one pmux server
//! and be adopted by the next. pmuxd and the holder talk over one named pipe
//! in both directions. Each frame is a kind byte, a little-endian `u32`
//! payload length, and the payload. The codec is plain bytes so it is tested
//! on every platform.

use std::io::{self, Read, Write};

/// The holder protocol this build speaks. A server and a holder must agree.
pub const HOLDER_PROTOCOL: u32 = 1;
/// Random bytes each holder is started with. A server must present them
/// before the holder trusts its pipe.
pub const TOKEN_LEN: usize = 32;
/// Upper bound on one payload. Output and input arrive in pieces well below it.
pub const MAX_PAYLOAD: usize = 1 << 20;

const HELLO: u8 = 1;
const READY: u8 = 2;
const OUTPUT: u8 = 3;
const INPUT: u8 = 4;
const RESIZE: u8 = 5;
const EXIT: u8 = 6;
const KILL: u8 = 7;
const HANDOFF: u8 = 8;

/// One message on a holder pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Server to holder, first on every connection.
    Hello {
        protocol: u32,
        token: [u8; TOKEN_LEN],
    },
    /// Holder to server: the token matched. `pid` is the pane's program.
    Ready { protocol: u32, pid: u32 },
    /// Holder to server: bytes the program wrote.
    Output(Vec<u8>),
    /// Server to holder: bytes for the program.
    Input(Vec<u8>),
    /// Server to holder: the pane's new size.
    Resize {
        cols: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    /// Holder to server: the program ended with this exit code.
    Exit { code: u32 },
    /// Server to holder: end the program and close the pseudoconsole.
    Kill,
    /// Server to holder: this server is about to hand the pane to the next
    /// one. Keep the program for `grace_ms` after the pipe closes.
    Handoff { grace_ms: u32 },
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn u32_at(payload: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(payload[at..at + 4].try_into().expect("four bytes"))
}

fn u16_at(payload: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(payload[at..at + 2].try_into().expect("two bytes"))
}

impl Frame {
    fn encode_payload(&self) -> (u8, Vec<u8>) {
        match self {
            Frame::Hello { protocol, token } => {
                let mut payload = protocol.to_le_bytes().to_vec();
                payload.extend_from_slice(token);
                (HELLO, payload)
            }
            Frame::Ready { protocol, pid } => {
                let mut payload = protocol.to_le_bytes().to_vec();
                payload.extend_from_slice(&pid.to_le_bytes());
                (READY, payload)
            }
            Frame::Output(bytes) => (OUTPUT, bytes.clone()),
            Frame::Input(bytes) => (INPUT, bytes.clone()),
            Frame::Resize {
                cols,
                rows,
                pixel_width,
                pixel_height,
            } => {
                let mut payload = Vec::with_capacity(8);
                for value in [cols, rows, pixel_width, pixel_height] {
                    payload.extend_from_slice(&value.to_le_bytes());
                }
                (RESIZE, payload)
            }
            Frame::Exit { code } => (EXIT, code.to_le_bytes().to_vec()),
            Frame::Kill => (KILL, Vec::new()),
            Frame::Handoff { grace_ms } => (HANDOFF, grace_ms.to_le_bytes().to_vec()),
        }
    }

    fn decode(kind: u8, payload: Vec<u8>) -> io::Result<Self> {
        let exact = |len: usize| {
            if payload.len() == len {
                Ok(())
            } else {
                Err(invalid("holder frame has the wrong length"))
            }
        };
        Ok(match kind {
            HELLO => {
                exact(4 + TOKEN_LEN)?;
                let mut token = [0; TOKEN_LEN];
                token.copy_from_slice(&payload[4..]);
                Frame::Hello {
                    protocol: u32_at(&payload, 0),
                    token,
                }
            }
            READY => {
                exact(8)?;
                Frame::Ready {
                    protocol: u32_at(&payload, 0),
                    pid: u32_at(&payload, 4),
                }
            }
            OUTPUT => Frame::Output(payload),
            INPUT => Frame::Input(payload),
            RESIZE => {
                exact(8)?;
                Frame::Resize {
                    cols: u16_at(&payload, 0),
                    rows: u16_at(&payload, 2),
                    pixel_width: u16_at(&payload, 4),
                    pixel_height: u16_at(&payload, 6),
                }
            }
            EXIT => {
                exact(4)?;
                Frame::Exit {
                    code: u32_at(&payload, 0),
                }
            }
            KILL => {
                exact(0)?;
                Frame::Kill
            }
            HANDOFF => {
                exact(4)?;
                Frame::Handoff {
                    grace_ms: u32_at(&payload, 0),
                }
            }
            _ => return Err(invalid("unknown holder frame")),
        })
    }
}

/// Write one frame. The caller flushes when it wants the bytes on the pipe.
pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let (kind, payload) = frame.encode_payload();
    if payload.len() > MAX_PAYLOAD {
        return Err(invalid("holder frame is too large"));
    }
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(&payload)
}

/// Read one frame. `Ok(None)` when the pipe closed between frames; a pipe
/// that closes inside a frame is `UnexpectedEof`.
pub fn read_frame(reader: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut kind = [0u8; 1];
    loop {
        match reader.read(&mut kind) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_PAYLOAD {
        return Err(invalid("holder frame is too large"));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Frame::decode(kind[0], payload).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_kind() -> Vec<Frame> {
        vec![
            Frame::Hello {
                protocol: HOLDER_PROTOCOL,
                token: [7; TOKEN_LEN],
            },
            Frame::Ready {
                protocol: HOLDER_PROTOCOL,
                pid: 4242,
            },
            Frame::Output(b"hello\r\n\x1b[1mbold".to_vec()),
            Frame::Output(Vec::new()),
            Frame::Input(b"ls\r".to_vec()),
            Frame::Resize {
                cols: 120,
                rows: 40,
                pixel_width: 1200,
                pixel_height: 800,
            },
            Frame::Exit { code: 0xC000_013A },
            Frame::Kill,
            Frame::Handoff { grace_ms: 30_000 },
        ]
    }

    #[test]
    fn every_frame_survives_a_round_trip_in_one_stream() {
        let mut wire = Vec::new();
        for frame in every_kind() {
            write_frame(&mut wire, &frame).unwrap();
        }
        let mut reader = wire.as_slice();
        let mut read = Vec::new();
        while let Some(frame) = read_frame(&mut reader).unwrap() {
            read.push(frame);
        }
        assert_eq!(read, every_kind());
    }

    #[test]
    fn the_wire_layout_is_kind_length_payload() {
        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            &Frame::Resize {
                cols: 0x0102,
                rows: 3,
                pixel_width: 0,
                pixel_height: 0xFFFF,
            },
        )
        .unwrap();
        assert_eq!(
            wire,
            [RESIZE, 8, 0, 0, 0, 0x02, 0x01, 3, 0, 0, 0, 0xFF, 0xFF]
        );
    }

    #[test]
    fn a_pipe_closed_inside_a_frame_is_unexpected_eof() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &Frame::Output(b"truncated".to_vec())).unwrap();
        for cut in 1..wire.len() {
            let err = read_frame(&mut &wire[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "cut at {cut}");
        }
        assert_eq!(read_frame(&mut &[][..]).unwrap(), None);
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let unknown = [99u8, 0, 0, 0, 0];
        assert_eq!(
            read_frame(&mut &unknown[..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let short_exit = [EXIT, 2, 0, 0, 0, 1, 2];
        assert_eq!(
            read_frame(&mut &short_exit[..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut huge = vec![OUTPUT];
        huge.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert_eq!(
            read_frame(&mut huge.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "an oversized length must fail before allocating"
        );
        let too_big = Frame::Input(vec![0; MAX_PAYLOAD + 1]);
        assert_eq!(
            write_frame(&mut Vec::new(), &too_big).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
