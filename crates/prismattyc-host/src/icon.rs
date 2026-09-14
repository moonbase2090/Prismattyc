//! App icon for the windowed host (brand assets under `assets/brand/`).

use winit::window::Icon;

/// Decode the brand PNG embedded at compile time into a winit [`Icon`].
///
/// Returns `None` only if decode fails (should not happen for checked-in assets).
pub fn load_window_icon() -> Option<Icon> {
    // Dark squircle tile — same mark Linux launchers and the window/taskbar
    // use. Transparent prismattyc-128.png sits on the DE's light chrome and
    // does not read as the macOS Dock icon. (see install-prismattyc-host-desktop.sh).
    const PNG: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/brand/png/prismattyc-tile-128.png"
    ));
    decode_png_icon(PNG)
}

fn decode_png_icon(png_bytes: &[u8]) -> Option<Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let width = info.width;
    let height = info.height;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let rgb = &buf[..info.buffer_size()];
            let mut out = Vec::with_capacity((rgb.len() / 3) * 4);
            for chunk in rgb.chunks_exact(3) {
                out.extend_from_slice(chunk);
                out.push(255);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let ga = &buf[..info.buffer_size()];
            let mut out = Vec::with_capacity((ga.len() / 2) * 4);
            for chunk in ga.chunks_exact(2) {
                let g = chunk[0];
                out.extend_from_slice(&[g, g, g, chunk[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let g = &buf[..info.buffer_size()];
            let mut out = Vec::with_capacity(g.len() * 4);
            for &v in g {
                out.extend_from_slice(&[v, v, v, 255]);
            }
            out
        }
        other => {
            eprintln!("prismattyc-host: unsupported icon color type {other:?}");
            return None;
        }
    };
    match Icon::from_rgba(rgba, width, height) {
        Ok(icon) => Some(icon),
        Err(error) => {
            eprintln!("prismattyc-host: window icon: {error}");
            None
        }
    }
}

/// Set the Dock tile from the dark macOS-grid brand raster.
///
/// winit `with_window_icon` is ignored on macOS. The SVG master lives at
/// `assets/brand/macos/prismattyc.svg`; this PNG is that mark on a dark
/// rounded-squircle tile, matching native macOS icons and the
/// Prismattyc.app `.icns` the install script builds.
#[cfg(target_os = "macos")]
pub fn apply_macos_app_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    const PNG: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/brand/png/prismattyc-tile-1024.png"
    ));
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("prismattyc-host: macos dock icon: not on the main thread");
        return;
    };
    let data = NSData::with_bytes(PNG);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        eprintln!("prismattyc-host: macos dock icon: NSImage failed");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: main-thread NSApp after winit created the window.
    unsafe {
        app.setApplicationIconImage(Some(&image));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_icon_decodes() {
        let icon = load_window_icon().expect("brand PNG should decode");
        // Icon is opaque; successful from_rgba is enough.
        let _ = icon;
    }

    #[test]
    fn macos_dock_raster_decodes() {
        const PNG: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/brand/png/prismattyc-tile-1024.png"
        ));
        let icon = decode_png_icon(PNG).expect("dark 1024 brand PNG should decode");
        let _ = icon;
    }

    #[test]
    fn macos_svg_master_is_present() {
        const SVG: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/brand/macos/prismattyc.svg"
        ));
        assert!(SVG.contains("viewBox=\"0 0 512 512\""), "{SVG}");
        assert!(SVG.contains("#62A8FF"), "spectrum blue missing");
        assert!(SVG.contains("#7B8CFA"), "spectrum indigo missing");
    }

    #[test]
    fn desktop_tile_svg_is_present() {
        const SVG: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/brand/prismattyc-icon-tile.svg"
        ));
        assert!(SVG.contains("viewBox=\"0 0 512 512\""), "{SVG}");
        assert!(SVG.contains("#121214"), "tile fill missing");
        assert!(SVG.contains("#62A8FF"), "spectrum blue missing");
        assert!(SVG.contains("#7B8CFA"), "spectrum indigo missing");
    }
}
