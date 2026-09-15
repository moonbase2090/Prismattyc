//! Bounded image updates for the retained macOS compositor layers.

use crate::frame_damage::{FrameDamage, PixelRect};

// Short tiles limit the copy cost of terminal row and cursor updates. Width
// bounds vertical chrome updates without creating thousands of CALayers.
const TILE_WIDTH: usize = 512;
const TILE_HEIGHT: usize = 128;

pub(crate) fn tiles(width: usize, height: usize) -> Vec<PixelRect> {
    (0..height)
        .step_by(TILE_HEIGHT)
        .flat_map(|y| {
            (0..width).step_by(TILE_WIDTH).map(move |x| {
                PixelRect::new(x, y, TILE_WIDTH.min(width - x), TILE_HEIGHT.min(height - y))
            })
        })
        .collect()
}

pub(crate) fn damaged_tiles(tiles: &[PixelRect], damage: &FrameDamage) -> Vec<usize> {
    tiles
        .iter()
        .enumerate()
        .filter_map(|(index, tile)| {
            let dirty = match damage {
                FrameDamage::Full => true,
                FrameDamage::Rects(rects) => rects.iter().any(|rect| {
                    rect.width > 0
                        && rect.height > 0
                        && rect.x < tile.x + tile.width
                        && rect.y < tile.y + tile.height
                        && rect.x.saturating_add(rect.width) > tile.x
                        && rect.y.saturating_add(rect.height) > tile.y
                }),
            };
            dirty.then_some(index)
        })
        .collect()
}

/// Copy straight ARGB into a packed tile. The retained framebuffer is never
/// premultiplied, so later partial paints cannot darken unchanged pixels.
pub(crate) fn copy_tile(pixels: &[u32], stride: usize, tile: PixelRect, out: &mut Vec<u32>) {
    out.clear();
    out.reserve(tile.width * tile.height);
    for y in tile.y..tile.y + tile.height {
        let start = y * stride + tile.x;
        out.extend_from_slice(&pixels[start..start + tile.width]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::premultiply_in_place;

    #[test]
    fn full_frame_tiles_cover_edges_once() {
        for (width, height) in [(1, 1), (512, 128), (513, 129), (5186, 2740)] {
            let grid = tiles(width, height);
            let mut covered = vec![0_u8; width * height];
            for tile in &grid {
                assert!(tile.width <= TILE_WIDTH && tile.height <= TILE_HEIGHT);
                for y in tile.y..tile.y + tile.height {
                    for x in tile.x..tile.x + tile.width {
                        covered[y * width + x] += 1;
                    }
                }
            }
            assert!(covered.iter().all(|count| *count == 1));
            assert_eq!(damaged_tiles(&grid, &FrameDamage::Full).len(), grid.len());
        }
    }

    #[test]
    fn damage_handles_edges_overlap_empty_and_overflow() {
        let grid = tiles(1025, 257);
        let damage = FrameDamage::Rects(vec![
            PixelRect::new(511, 127, 2, 2),
            PixelRect::new(511, 127, 2, 2),
            PixelRect::new(1024, 256, usize::MAX, usize::MAX),
            PixelRect::new(1025, 0, 1, 1),
            PixelRect::new(0, 257, 1, 1),
            PixelRect::new(0, 0, 0, 1),
        ]);
        assert_eq!(damaged_tiles(&grid, &damage), [0, 1, 3, 4, 8]);
        assert!(damaged_tiles(&grid, &FrameDamage::rects()).is_empty());
    }

    #[test]
    fn cursor_update_converts_only_one_tile_at_large_window_size() {
        let grid = tiles(5186, 2740);
        let dirty = damaged_tiles(
            &grid,
            &FrameDamage::Rects(vec![PixelRect::new(100, 100, 12, 24)]),
        );
        assert_eq!(dirty, [0]);
        let copied: usize = dirty.iter().map(|i| grid[*i].width * grid[*i].height).sum();
        assert!(copied * 200 < 5186 * 2740);
    }

    #[test]
    fn partial_publication_matches_full_image_and_keeps_straight_source() {
        let (width, height) = (1025, 257);
        let grid = tiles(width, height);
        let mut pixels = vec![0x80402010; width * height];
        let mut packed = Vec::new();
        let mut retained = vec![0; pixels.len()];
        for damage in [
            FrameDamage::Full,
            FrameDamage::Rects(vec![PixelRect::new(510, 126, 5, 5)]),
            FrameDamage::rects(),
        ] {
            if let FrameDamage::Rects(rects) = &damage {
                for rect in rects {
                    for y in rect.y..rect.y + rect.height {
                        pixels[y * width + rect.x..y * width + rect.x + rect.width]
                            .fill(0xffabcdef);
                    }
                }
            }
            let before = pixels.clone();
            for index in damaged_tiles(&grid, &damage) {
                let tile = grid[index];
                copy_tile(&pixels, width, tile, &mut packed);
                premultiply_in_place(&mut packed);
                for row in 0..tile.height {
                    let start = (tile.y + row) * width + tile.x;
                    retained[start..start + tile.width]
                        .copy_from_slice(&packed[row * tile.width..(row + 1) * tile.width]);
                }
            }
            assert_eq!(pixels, before);
            let mut expected = pixels.clone();
            premultiply_in_place(&mut expected);
            assert_eq!(retained, expected);
            assert_eq!(retained[0], 0x80201008);
        }
    }
}
