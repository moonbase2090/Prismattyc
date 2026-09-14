//! Minimal RFC 4648 base64 decoder for Kitty graphics payloads.
//! Hand-rolled to avoid a new dependency (matches the no-dep-CLI style).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Base64Error {
    BadChar(u8),
    BadLength,
}

const INVALID: u8 = 0xFF;
const PAD: u8 = 0xFE;

fn value(byte: u8) -> u8 {
    match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        b'=' => PAD,
        _ => INVALID,
    }
}

pub(crate) fn decode(input: &[u8]) -> Result<Vec<u8>, Base64Error> {
    // Collect non-whitespace symbols first (Kitty may wrap payloads).
    let mut syms: Vec<u8> = Vec::with_capacity(input.len());
    for &b in input {
        if b.is_ascii_whitespace() {
            continue;
        }
        let v = value(b);
        if v == INVALID {
            return Err(Base64Error::BadChar(b));
        }
        syms.push(v);
    }
    if !syms.len().is_multiple_of(4) {
        return Err(Base64Error::BadLength);
    }
    let mut out = Vec::with_capacity(syms.len() / 4 * 3);
    for chunk in syms.chunks(4) {
        let pads = chunk.iter().filter(|&&v| v == PAD).count();
        // Padding may only appear in the final positions.
        if pads > 2 || (pads > 0 && chunk[0] == PAD) || (pads == 2 && chunk[1] == PAD) {
            return Err(Base64Error::BadLength);
        }
        let b0 = chunk[0];
        let b1 = chunk[1];
        let b2 = if chunk[2] == PAD { 0 } else { chunk[2] };
        let b3 = if chunk[3] == PAD { 0 } else { chunk[3] };
        out.push((b0 << 2) | (b1 >> 4));
        if chunk[2] != PAD {
            out.push((b1 << 4) | (b2 >> 2));
        }
        if chunk[3] != PAD {
            out.push((b2 << 6) | b3);
        }
    }
    Ok(out)
}

/// Encode bytes as standard base64 (test helper only; the decoder above is
/// the production path).
#[cfg(all(test, unix))]
pub(crate) fn base64_encode_for_test(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_known_vectors() {
        assert_eq!(decode(b"").unwrap(), b"");
        assert_eq!(decode(b"Zg==").unwrap(), b"f");
        assert_eq!(decode(b"Zm8=").unwrap(), b"fo");
        assert_eq!(decode(b"Zm9v").unwrap(), b"foo");
        assert_eq!(decode(b"Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn ignores_ascii_whitespace() {
        assert_eq!(decode(b"Zm9v\r\n YmFy").unwrap(), b"foobar");
    }

    #[test]
    fn rejects_bad_char_and_length() {
        assert!(matches!(decode(b"Zm9*"), Err(Base64Error::BadChar(b'*'))));
        assert!(matches!(decode(b"Zm9"), Err(Base64Error::BadLength)));
    }
}
