//! Pixel conversion shared by presentation backends and native probes.

/// Premultiply every pixel's RGB by its alpha, in place.
///
/// X11 (and Wayland where a backend carries alpha) expects premultiplied
/// ARGB. Skip the pass entirely when the window is opaque.
pub fn premultiply_in_place(buffer: &mut [u32]) {
    for px in buffer.iter_mut() {
        let alpha = *px >> 24;
        if alpha == 255 {
            continue;
        }
        let r = ((*px >> 16) & 0xff) * alpha / 255;
        let g = ((*px >> 8) & 0xff) * alpha / 255;
        let b = (*px & 0xff) * alpha / 255;
        *px = (alpha << 24) | (r << 16) | (g << 8) | b;
    }
}
