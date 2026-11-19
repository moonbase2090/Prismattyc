// SPDX-License-Identifier: MIT
//! Native images use straight RGBA in byte-encoded sRGB, with nearest sampling.

use std::num::NonZeroU32;

pub fn source_over(source: [u8; 4], destination: [u8; 4], opacity: u8) -> [u8; 4] {
    let alpha = u64::from(source[3]) * u64::from(opacity) / 255;
    if destination[3] == 255 {
        let mut result = [0, 0, 0, 255];
        for channel in 0..3 {
            result[channel] = ((u64::from(source[channel]) * alpha
                + u64::from(destination[channel]) * (255 - alpha)
                + 127)
                / 255) as u8;
        }
        return result;
    }
    let destination_alpha = u64::from(destination[3]);
    let numerator = alpha * 255 + destination_alpha * (255 - alpha);
    if numerator == 0 {
        return [0; 4];
    }
    let mut result = [0; 4];
    result[3] = ((numerator + 127) / 255) as u8;
    for channel in 0..3 {
        result[channel] = ((u64::from(source[channel]) * alpha * 255
            + u64::from(destination[channel]) * destination_alpha * (255 - alpha)
            + numerator / 2)
            / numerator) as u8;
    }
    result
}

/// One original destination axis, retained unchanged when the paint loop clips.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplingAxis {
    source_start: u32,
    source_length: NonZeroU32,
    destination_start: i32,
    destination_length: NonZeroU32,
}

impl SamplingAxis {
    pub fn new(
        source_start: u32,
        source_length: u32,
        source_limit: u32,
        destination_start: i32,
        destination_length: u32,
    ) -> Option<Self> {
        const LIMIT: i64 = 1_073_741_824;
        let source_end = source_start.checked_add(source_length)?;
        let destination_end = i64::from(destination_start) + i64::from(destination_length);
        if source_end > source_limit
            || destination_length > LIMIT as u32
            || i64::from(destination_start) < -LIMIT
            || destination_end > LIMIT
        {
            return None;
        }
        Some(Self {
            source_start,
            source_length: NonZeroU32::new(source_length)?,
            destination_start,
            destination_length: NonZeroU32::new(destination_length)?,
        })
    }

    pub fn at(self, pixel: i32) -> Option<u32> {
        let relative = i64::from(pixel) - i64::from(self.destination_start);
        if relative < 0 || relative >= i64::from(self.destination_length.get()) {
            return None;
        }
        let offset = relative as u64 * u64::from(self.source_length.get())
            / u64::from(self.destination_length.get());
        // Construction proves that the entire source interval fits u32.
        Some((u64::from(self.source_start) + offset) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_composition_matches_the_six_reviewed_rgba_literals() {
        for (source, destination, opacity, expected) in [
            (0xff000080u32, 0x0000ffffu32, 255, 0x80007fffu32),
            (0x0164ff80, 0x000000ff, 255, 0x013280ff),
            (0xff000080, 0x0a141eff, 128, 0x470f16ff),
            (0xff000080, 0x0000ff80, 255, 0xaa0055c0),
            (0x07080900, 0xc8966400, 255, 0x00000000),
            (0x0a141eff, 0xc8966401, 0, 0xc8966401),
        ] {
            assert_eq!(
                source_over(source.to_be_bytes(), destination.to_be_bytes(), opacity),
                expected.to_be_bytes()
            );
        }
    }

    #[test]
    fn clipping_keeps_coordinates_from_the_original_destination() {
        let axis = SamplingAxis::new(3, 5, 8, -2, 4).unwrap();
        assert_eq!(axis.at(-3), None);
        assert_eq!(axis.at(-2), Some(3));
        assert_eq!(axis.at(-1), Some(4));
        assert_eq!(axis.at(0), Some(5));
        assert_eq!(axis.at(1), Some(6));
        assert_eq!(axis.at(2), None);
        // A guest clipped to [0,2) samples 5 and 6, retaining the same axis.
        assert_eq!(
            (0..2).map(|pixel| axis.at(pixel)).collect::<Vec<_>>(),
            [Some(5), Some(6)]
        );
    }

    #[test]
    fn nearest_upscale_samples_the_original_crop_without_rounding() {
        let axis = SamplingAxis::new(2, 3, 6, 0, 8).unwrap();
        assert_eq!(
            (0..8).map(|pixel| axis.at(pixel)).collect::<Vec<_>>(),
            [
                Some(2),
                Some(2),
                Some(2),
                Some(3),
                Some(3),
                Some(3),
                Some(4),
                Some(4)
            ]
        );
    }

    #[test]
    fn sampling_constructor_rejects_invalid_source_and_destination_intervals() {
        for (start, length, limit, destination, span) in [
            (0, 0, 1, 0, 1),
            (0, 1, 1, 0, 0),
            (7, 2, 8, 0, 1),
            (u32::MAX, 1, u32::MAX, 0, 1),
            (0, 1, 1, -1_073_741_825, 1),
            (0, 1, 1, 1_073_741_824, 1),
            (0, 1, 1, 0, 1_073_741_825),
        ] {
            assert_eq!(
                SamplingAxis::new(start, length, limit, destination, span),
                None
            );
        }
        let edge = SamplingAxis::new(0, 1, 1, -1_073_741_824, 1_073_741_824).unwrap();
        assert_eq!(edge.at(-1_073_741_824), Some(0));
        assert_eq!(edge.at(-1), Some(0));
        assert_eq!(edge.at(0), None);
    }
}
