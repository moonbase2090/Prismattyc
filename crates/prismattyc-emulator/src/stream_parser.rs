// SPDX-License-Identifier: MPL-2.0
//! Complete a split UTF-8 scalar without allowing parser lookahead to consume
//! following characters. Ordinary buffers still use VTE's bulk advance path.

pub(super) struct StreamParser {
    inner: vte::Parser,
    continuations: u8,
}

impl StreamParser {
    pub(super) fn new() -> Self {
        Self {
            inner: vte::Parser::new(),
            continuations: 0,
        }
    }

    pub(super) fn advance<P: vte::Perform>(&mut self, performer: &mut P, mut bytes: &[u8]) {
        // VTE 0.15's partial-scalar lookahead can consume an ASCII byte after
        // the completed scalar. Feed only the continuation prefix individually.
        // Tracking raw bytes is conservative in OSC/DCS payloads: splitting
        // those bytes into additional calls does not change their order.
        while self.continuations != 0 {
            let Some((&byte, rest)) = bytes.split_first() else {
                return;
            };
            self.inner.advance(performer, std::slice::from_ref(&byte));
            self.continuations = if (0x80..=0xbf).contains(&byte) {
                self.continuations - 1
            } else {
                continuation_count(byte)
            };
            bytes = rest;
        }
        self.inner.advance(performer, bytes);
        self.continuations = trailing_continuations(bytes);
    }
}

fn continuation_count(byte: u8) -> u8 {
    match byte {
        0xc2..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf4 => 3,
        _ => 0,
    }
}

fn trailing_continuations(bytes: &[u8]) -> u8 {
    // A pending scalar occupies at most three trailing bytes. An ASCII tail
    // takes one inspection; there is no additional full-buffer UTF-8 scan.
    for (seen, &byte) in bytes.iter().rev().take(3).enumerate() {
        if !(0x80..=0xbf).contains(&byte) {
            return continuation_count(byte).saturating_sub(seen as u8);
        }
    }
    0
}

#[cfg(test)]
mod utf8_chunk_tests {
    use super::*;

    struct Sink;
    impl vte::Perform for Sink {}

    #[test]
    fn pending_prefix_returns_to_bulk_at_each_scalar_boundary() {
        for text in ["¢", "߿", "ࠀ", "€", "\u{ffff}", "𐀀", "😀", "\u{10ffff}"] {
            let bytes = text.as_bytes();
            for split in 1..bytes.len() {
                let mut parser = StreamParser::new();
                parser.advance(&mut Sink, &bytes[..split]);
                assert_eq!(parser.continuations as usize, bytes.len() - split);
                parser.advance(&mut Sink, &[]);
                assert_eq!(parser.continuations as usize, bytes.len() - split);
                for (index, byte) in bytes[split..].iter().enumerate() {
                    parser.advance(&mut Sink, std::slice::from_ref(byte));
                    assert_eq!(
                        parser.continuations as usize,
                        bytes.len() - split - index - 1
                    );
                }
                parser.advance(&mut Sink, b"plain ASCII");
                assert_eq!(parser.continuations, 0);
            }
        }
        let mut parser = StreamParser::new();
        for byte in 0..=0x7f {
            parser.advance(&mut Sink, &[byte]);
            assert_eq!(parser.continuations, 0);
        }
    }
}
