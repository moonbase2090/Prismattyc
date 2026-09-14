//! Sidecar collector for Kitty graphics APC sequences: `ESC _ G <ctrl>;<payload> ESC \`.
//!
//! VTE swallows APC without delivering it to `Perform`, so we scan the raw
//! byte stream in parallel (same technique as `ApcCollector`, but scoped to
//! the `G` graphics namespace, with a much larger body cap for base64 pixels).

/// Total body-byte ceiling for a single graphics sequence. Oversized bodies
/// are dropped (no event) but drained to ST to keep the stream in sync.
pub const MAX_GRAPHICS_APC_BYTES: usize = 5_000_000;

/// A complete Kitty graphics command recovered from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsApc {
    /// Control data before the first `;`, with the leading `G` stripped.
    pub control: String,
    /// Raw (still base64) payload bytes after the first `;`.
    pub payload: Vec<u8>,
}

/// A complete graphics command and the byte offset where its APC terminator
/// was consumed. The offset is relative to the `push_with_offsets` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsApcEvent {
    pub apc: GraphicsApc,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    /// In an APC body; `is_graphics` set once we confirm the leading `G`.
    Body,
    BodyEsc,
}

#[derive(Debug, Default)]
pub struct GraphicsApcCollector {
    state: State,
    buffer: Vec<u8>,
    seen_first: bool,
    is_graphics: bool,
    overflow: bool,
}

impl GraphicsApcCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn is_active(&self) -> bool {
        !matches!(self.state, State::Ground)
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<GraphicsApc> {
        self.push_with_offsets(bytes)
            .into_iter()
            .map(|event| event.apc)
            .collect()
    }

    pub fn push_with_offsets(&mut self, bytes: &[u8]) -> Vec<GraphicsApcEvent> {
        let mut out = Vec::new();
        for (offset, &b) in bytes.iter().enumerate() {
            if let Some(apc) = self.push_byte(b) {
                out.push(GraphicsApcEvent {
                    apc,
                    end: offset + 1,
                });
            }
        }
        out
    }

    fn reset_body(&mut self) {
        self.buffer.clear();
        self.seen_first = false;
        self.is_graphics = false;
        self.overflow = false;
    }

    /// Handle a byte as APC body content: classify if first byte, then push if graphics.
    fn body_content(&mut self, byte: u8) {
        if !self.seen_first {
            self.seen_first = true;
            self.is_graphics = byte == b'G';
            // Do not store the leading 'G'.
            return;
        }
        if self.is_graphics {
            if self.buffer.len() >= MAX_GRAPHICS_APC_BYTES {
                self.overflow = true;
            } else if !self.overflow {
                self.buffer.push(byte);
            }
        }
    }

    fn push_byte(&mut self, byte: u8) -> Option<GraphicsApc> {
        match self.state {
            State::Ground => {
                if byte == 0x1b {
                    self.state = State::Esc;
                }
                None
            }
            State::Esc => {
                if byte == b'_' {
                    self.state = State::Body;
                    self.reset_body();
                } else if byte == 0x1b {
                    // Another ESC, stay in Esc (restart the escape sequence)
                    self.state = State::Esc;
                } else {
                    self.state = State::Ground;
                }
                None
            }
            State::Body => match byte {
                0x1b => {
                    self.state = State::BodyEsc;
                    None
                }
                _ => {
                    self.body_content(byte);
                    None
                }
            },
            State::BodyEsc => {
                if byte == b'\\' {
                    self.state = State::Ground;
                    let emit = self.is_graphics && !self.overflow;
                    let body = core::mem::take(&mut self.buffer);
                    self.reset_body();
                    if emit {
                        return Some(split_body(&body));
                    }
                    None
                } else if byte == 0x1b {
                    // Stray ESC: preserve it in the body and stay in BodyEsc for the next byte.
                    self.body_content(0x1b);
                    self.state = State::BodyEsc;
                    None
                } else {
                    // Not ST and not ESC: preserve the swallowed ESC and this byte.
                    self.body_content(0x1b);
                    self.body_content(byte);
                    self.state = State::Body;
                    None
                }
            }
        }
    }
}

fn split_body(body: &[u8]) -> GraphicsApc {
    match body.iter().position(|&b| b == b';') {
        Some(i) => GraphicsApc {
            control: String::from_utf8_lossy(&body[..i]).into_owned(),
            payload: body[i + 1..].to_vec(),
        },
        None => GraphicsApc {
            control: String::from_utf8_lossy(body).into_owned(),
            payload: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(bytes: &[u8]) -> Vec<GraphicsApc> {
        GraphicsApcCollector::new().push(bytes)
    }

    #[test]
    fn parses_transmit_and_display() {
        let ev = one(b"\x1b_Ga=T,t=d,f=100,q=2;SGk=\x1b\\");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].control, "a=T,t=d,f=100,q=2");
        assert_eq!(ev[0].payload, b"SGk=");
    }

    #[test]
    fn parses_query_with_no_semicolon_payload() {
        // Query: control then ';' then a tiny base64 pixel.
        let ev = one(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].control, "i=31,s=1,v=1,a=q,t=d,f=24");
        assert_eq!(ev[0].payload, b"AAAA");
    }

    #[test]
    fn ignores_non_graphics_apc() {
        // A Prismattyc-namespace APC must NOT be captured by the graphics collector.
        let ev = one(b"\x1b_Prismattyc;cap;foo\x1b\\");
        assert!(ev.is_empty());
    }

    #[test]
    fn reassembles_split_across_pushes() {
        let mut c = GraphicsApcCollector::new();
        assert!(c.push(b"\x1b_Ga=T,t=d,f=100;SG").is_empty());
        assert!(c.is_active());
        let ev = c.push(b"k=\x1b\\");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].payload, b"SGk=");
    }

    #[test]
    fn reports_apc_end_offsets() {
        let bytes = b"prefix\x1b_Ga=T,t=d,f=100;SGk=\x1b\\suffix";
        let events = GraphicsApcCollector::new().push_with_offsets(bytes);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].end, bytes.len() - b"suffix".len());
        assert_eq!(events[0].apc.control, "a=T,t=d,f=100");
    }

    #[test]
    fn preserves_stray_esc_in_body() {
        // Stray ESC not followed by \ should be preserved in the body.
        let ev = one(b"\x1b_Ga=1\x1bb=2\x1b\\");
        assert_eq!(ev.len(), 1);
        // The stray ESC (0x1b) should be preserved in the control string
        let control_bytes = ev[0].control.as_bytes();
        assert_eq!(control_bytes, b"a=1\x1bb=2");
        assert_eq!(ev[0].payload, b"");
    }

    #[test]
    fn stray_esc_as_first_content_byte() {
        // Stray ESC as first byte after G classification should not break classification.
        let ev = one(b"\x1b_G\x1bX\x1b\\");
        assert_eq!(ev.len(), 1);
        // Should classify as graphics and include the ESC in the control
        let control_bytes = ev[0].control.as_bytes();
        assert_eq!(control_bytes, b"\x1bX");
        assert_eq!(ev[0].payload, b"");
    }
}
