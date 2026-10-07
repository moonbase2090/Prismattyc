//! Pixel conversion shared by presentation backends and native probes.

/// Premultiply every pixel's RGB by its alpha, in place.
///
/// X11 (and Wayland where a backend carries alpha) expects premultiplied
/// ARGB. Skip opaque chunks before converting any translucent pixels.
pub fn premultiply_in_place(buffer: &mut [u32]) {
    const CHUNK_PIXELS: usize = 8;
    let full_len = buffer.len() / CHUNK_PIXELS * CHUNK_PIXELS;
    let mut cursor = 0;

    while cursor < full_len {
        let chunk_end = cursor + CHUNK_PIXELS;
        if chunk_is_opaque(&buffer[cursor..chunk_end]) {
            cursor = chunk_end;
            continue;
        }

        let run_start = cursor;
        cursor = chunk_end;
        while cursor < full_len && !chunk_is_opaque(&buffer[cursor..cursor + CHUNK_PIXELS]) {
            cursor += CHUNK_PIXELS;
        }
        premultiply_branchless(&mut buffer[run_start..cursor]);
    }
    premultiply_branchless(&mut buffer[full_len..]);
}

fn chunk_is_opaque(chunk: &[u32]) -> bool {
    chunk.iter().all(|pixel| *pixel >> 24 == 0xff)
}

fn premultiply_branchless(buffer: &mut [u32]) {
    for pixel in buffer {
        let alpha = *pixel >> 24;
        let red = premultiply_channel((*pixel >> 16) & 0xff, alpha);
        let green = premultiply_channel((*pixel >> 8) & 0xff, alpha);
        let blue = premultiply_channel(*pixel & 0xff, alpha);
        *pixel = (alpha << 24) | (red << 16) | (green << 8) | blue;
    }
}

#[inline]
fn premultiply_channel(channel: u32, alpha: u32) -> u32 {
    let product = channel * alpha;
    (product + 1 + (product >> 8)) >> 8
}

#[cfg(test)]
mod tests {
    use super::premultiply_in_place;

    fn reference_premultiply_in_place(buffer: &mut [u32]) {
        for pixel in buffer.iter_mut() {
            let alpha = *pixel >> 24;
            if alpha == 255 {
                continue;
            }
            let red = ((*pixel >> 16) & 0xff) * alpha / 255;
            let green = ((*pixel >> 8) & 0xff) * alpha / 255;
            let blue = (*pixel & 0xff) * alpha / 255;
            *pixel = (alpha << 24) | (red << 16) | (green << 8) | blue;
        }
    }

    #[test]
    fn premultiply_matches_reference_for_alpha_and_channel_values() {
        let mut input = Vec::with_capacity(3 * 256 * 256 + 4096);
        for alpha in 0..=255u32 {
            for channel in 0..=255u32 {
                input.extend([
                    (alpha << 24) | (channel << 16) | (0x5a << 8) | 0xa5,
                    (alpha << 24) | (0x3c << 16) | (channel << 8) | 0xa5,
                    (alpha << 24) | (0x3c << 16) | (0x5a << 8) | channel,
                ]);
            }
        }

        let mut state = 0x9e37_79b9u32;
        for _ in 0..4096 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            input.push(state);
        }

        let mut expected = input.clone();
        reference_premultiply_in_place(&mut expected);
        premultiply_in_place(&mut input);
        if let Some((index, (actual, expected))) = input
            .iter()
            .zip(&expected)
            .enumerate()
            .find(|(_, (actual, expected))| actual != expected)
        {
            panic!("pixel {index}: got {actual:#010x}, expected {expected:#010x}");
        }

        for length in 0..=17 {
            let mut input: Vec<u32> = (0..length)
                .map(|index| {
                    let alpha = match index % 3 {
                        0 => 0xff,
                        1 => 0x80,
                        _ => 0,
                    };
                    alpha << 24 | (index as u32 * 0x0102_03) & 0x00ff_ffff
                })
                .collect();
            let mut expected = input.clone();
            reference_premultiply_in_place(&mut expected);
            premultiply_in_place(&mut input);
            assert_eq!(input, expected, "slice length {length}");
        }
    }
}
