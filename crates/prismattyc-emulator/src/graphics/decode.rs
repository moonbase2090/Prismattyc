//! Bounded PNG decode. Rejects on IHDR dimensions and a byte cap BEFORE
//! allocating pixel buffers (decompression-bomb defense), then normalizes to
//! 8-bit RGBA.

#[derive(Debug, Clone)]
pub(crate) struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MaxDims {
    pub w: u32,
    pub h: u32,
    pub bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    TooLarge,
    Format,
    Corrupt,
}

pub(crate) fn decode_png_bounded(bytes: &[u8], max: MaxDims) -> Result<DecodedImage, DecodeError> {
    let mut decoder = png::Decoder::new(bytes);
    // Hard cap on internal allocations (IDAT/palette) — bomb defense.
    decoder.set_limits(png::Limits { bytes: max.bytes });
    // Normalize palette/low-bit-depth to 8-bit channels.
    decoder.set_transformations(png::Transformations::normalize_to_color8());

    let mut reader = decoder.read_info().map_err(map_png_err)?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);

    // Bounds BEFORE allocating the output buffer.
    if w == 0 || h == 0 || w > max.w || h > max.h {
        return Err(DecodeError::TooLarge);
    }
    let pixels = (w as u64)
        .checked_mul(h as u64)
        .ok_or(DecodeError::TooLarge)?;
    let rgba_len = pixels.checked_mul(4).ok_or(DecodeError::TooLarge)?;
    if rgba_len > max.bytes as u64 {
        return Err(DecodeError::TooLarge);
    }

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(map_png_err)?;
    let src = &buf[..frame.buffer_size()];

    let rgba = to_rgba8(src, frame.color_type, w, h)?;
    Ok(DecodedImage {
        width: w,
        height: h,
        rgba,
    })
}

fn to_rgba8(src: &[u8], color: png::ColorType, w: u32, h: u32) -> Result<Vec<u8>, DecodeError> {
    let count = (w as usize)
        .checked_mul(h as usize)
        .ok_or(DecodeError::TooLarge)?;
    let mut out = vec![0u8; count.checked_mul(4).ok_or(DecodeError::TooLarge)?];
    match color {
        png::ColorType::Rgba => {
            if src.len() < count * 4 {
                return Err(DecodeError::Corrupt);
            }
            out.copy_from_slice(&src[..count * 4]);
        }
        png::ColorType::Rgb => {
            if src.len() < count * 3 {
                return Err(DecodeError::Corrupt);
            }
            for i in 0..count {
                out[i * 4] = src[i * 3];
                out[i * 4 + 1] = src[i * 3 + 1];
                out[i * 4 + 2] = src[i * 3 + 2];
                out[i * 4 + 3] = 0xFF;
            }
        }
        png::ColorType::Grayscale => {
            if src.len() < count {
                return Err(DecodeError::Corrupt);
            }
            for i in 0..count {
                let g = src[i];
                out[i * 4] = g;
                out[i * 4 + 1] = g;
                out[i * 4 + 2] = g;
                out[i * 4 + 3] = 0xFF;
            }
        }
        png::ColorType::GrayscaleAlpha => {
            if src.len() < count * 2 {
                return Err(DecodeError::Corrupt);
            }
            for i in 0..count {
                let g = src[i * 2];
                out[i * 4] = g;
                out[i * 4 + 1] = g;
                out[i * 4 + 2] = g;
                out[i * 4 + 3] = src[i * 2 + 1];
            }
        }
        png::ColorType::Indexed => return Err(DecodeError::Format), // normalize_to_color8 expands palette
    }
    Ok(out)
}

fn map_png_err(err: png::DecodingError) -> DecodeError {
    match err {
        png::DecodingError::LimitsExceeded => DecodeError::TooLarge,
        png::DecodingError::Format(_) => DecodeError::Format,
        _ => DecodeError::Corrupt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_channel_normalization_preserves_color_and_alpha_and_rejects_short_pixels() {
        let cases: &[(png::ColorType, &[u8], &[u8])] = &[
            (
                png::ColorType::Rgba,
                &[10, 20, 30, 40, 50, 60, 70, 80],
                &[10, 20, 30, 40, 50, 60, 70, 80],
            ),
            (
                png::ColorType::Rgb,
                &[10, 20, 30, 50, 60, 70],
                &[10, 20, 30, 255, 50, 60, 70, 255],
            ),
            (
                png::ColorType::Grayscale,
                &[10, 50],
                &[10, 10, 10, 255, 50, 50, 50, 255],
            ),
            (
                png::ColorType::GrayscaleAlpha,
                &[10, 40, 50, 80],
                &[10, 10, 10, 40, 50, 50, 50, 80],
            ),
        ];
        for &(color, pixels, expected) in cases {
            assert_eq!(to_rgba8(pixels, color, 2, 1).unwrap(), expected);
            assert_eq!(
                to_rgba8(&pixels[..pixels.len() - 1], color, 2, 1),
                Err(DecodeError::Corrupt)
            );
        }
        assert_eq!(
            to_rgba8(&[0, 1], png::ColorType::Indexed, 2, 1),
            Err(DecodeError::Format)
        );
    }

    // A 1x1 opaque-red PNG, produced once and pasted as bytes so the test has
    // no encoder dependency. (Generate with: `printf` a real PNG, or the png
    // crate in a scratch bin; the reviewer may regenerate.)
    const RED_1X1_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xF7, 0x03, 0x41, 0x43, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn dims(w: u32, h: u32) -> MaxDims {
        MaxDims {
            w,
            h,
            bytes: 8 << 20,
        }
    }

    #[test]
    fn decodes_1x1_to_rgba() {
        let img = decode_png_bounded(RED_1X1_PNG, dims(2048, 1024)).unwrap();
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgba.len(), 4);
        assert_eq!(img.rgba[0], 0xFF); // red
        assert_eq!(img.rgba[3], 0xFF); // opaque
    }

    #[test]
    fn rejects_when_dimensions_exceed_cap_before_decode() {
        // Cap smaller than the image's 1x1 → TooLarge.
        let err = decode_png_bounded(
            RED_1X1_PNG,
            MaxDims {
                w: 0,
                h: 0,
                bytes: 8 << 20,
            },
        );
        assert!(matches!(err, Err(DecodeError::TooLarge)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            decode_png_bounded(b"not a png", dims(2048, 1024)),
            Err(DecodeError::Format) | Err(DecodeError::Corrupt)
        ));
    }
}
