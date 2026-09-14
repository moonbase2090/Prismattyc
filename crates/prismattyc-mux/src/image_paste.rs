//! Clipboard image → temp PNG → child paste text.
//!
//! Agent CLIs cannot consume `image/png` on a PTY. paste is text-only;
//! an image-only clipboard currently fails `get_text` (windowed host) or
//! arrives as an empty bracketed paste (mux-attach). Cursor then shows
//! `[image omitted]`. Save a PNG under `$XDG_RUNTIME_DIR/prism-paste/` and
//! paste a filesystem path (`@path` for cursor-agent).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::inject_submit::{detect_inject_agent, InjectAgent};

const MAX_PIXELS: usize = 16_777_216;
const MAX_KEEP: usize = 8;
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Directory for host-written paste images.
#[must_use]
pub fn paste_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir);
    base.join("prism-paste")
}

/// Encode packed RGBA8 into a PNG.
pub fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
    let pixels = (width as usize).saturating_mul(height as usize);
    anyhow::ensure!(width > 0 && height > 0, "empty image");
    anyhow::ensure!(pixels <= MAX_PIXELS, "image exceeds {MAX_PIXELS} pixels");
    anyhow::ensure!(rgba.len() >= pixels.saturating_mul(4), "short RGBA buffer");
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgba[..pixels * 4])?;
    }
    Ok(buf)
}

/// Write `rgba` into `dir` as `<hex>.png`. Prunes older files past [`MAX_KEEP`].
pub fn write_paste_png_in(
    dir: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> anyhow::Result<PathBuf> {
    let png = encode_rgba_png(width, height, rgba)?;
    fs::create_dir_all(dir)?;
    prune_old(dir);
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|err| anyhow::anyhow!("getrandom: {err}"))?;
    let path = dir.join(format!("{}.png", hex_lower(&nonce)));
    let mut file = fs::File::create(&path)?;
    file.write_all(&png)?;
    file.sync_all()?;
    prune_old(dir);
    Ok(path)
}

/// Write into [`paste_dir`].
pub fn write_paste_png(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<PathBuf> {
    write_paste_png_in(&paste_dir(), width, height, rgba)
}

/// Read `image/png` (or arboard RGBA) from an existing clipboard and save it.
#[must_use]
pub fn clipboard_image_to_png_with(clipboard: &mut arboard::Clipboard) -> Option<PathBuf> {
    let image = clipboard.get_image().ok()?;
    let width = u32::try_from(image.width).ok()?;
    let height = u32::try_from(image.height).ok()?;
    write_paste_png(width, height, image.bytes.as_ref()).ok()
}

/// Read one image file path from an existing clipboard file list.
#[must_use]
pub fn clipboard_image_file_with(clipboard: &mut arboard::Clipboard) -> Option<PathBuf> {
    let paths = clipboard.get().file_list().ok()?;
    single_image_path(paths)
}

/// Parse a `text/uri-list` payload into local file paths.
#[must_use]
pub fn parse_file_uri_list(text: &str) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let encoded = line.strip_prefix("file://")?;
        let decoded = percent_decode(encoded)?;
        if decoded.is_empty() {
            return None;
        }
        paths.push(PathBuf::from(decoded));
    }
    (!paths.is_empty()).then_some(paths)
}

/// Return one image path from a `text/uri-list` payload.
#[must_use]
pub fn image_path_from_uri_list(text: &str) -> Option<PathBuf> {
    single_image_path(parse_file_uri_list(text)?)
}

fn single_image_path(paths: Vec<PathBuf>) -> Option<PathBuf> {
    let [path] = paths.as_slice() else {
        return None;
    };
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
    )
    .then(|| path.clone())
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = bytes.get(index + 1).and_then(|byte| hex_value(*byte))?;
            let lo = bytes.get(index + 2).and_then(|byte| hex_value(*byte))?;
            decoded.push((hi << 4) | lo);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Text the child should see. Cursor attaches with `@path`.
#[must_use]
pub fn paste_reference(path: &Path, agent: InjectAgent) -> String {
    let displayed = path.display().to_string();
    match agent {
        InjectAgent::Cursor => format!("@{displayed}"),
        _ => displayed,
    }
}

/// True when paste is empty after stripping bracketed-paste wrappers.
#[must_use]
pub fn is_empty_bracketed_or_blank(text: &str) -> bool {
    strip_bracketed(text).trim().is_empty()
}

/// True for an empty DECSET 2004 paste (`CSI 200~` … `CSI 201~`), not a
/// lone space or CR from the typing path.
#[must_use]
pub fn is_empty_bracketed_paste(text: &str) -> bool {
    text.contains(PASTE_START) && text.contains(PASTE_END) && is_empty_bracketed_or_blank(text)
}

/// If `text` is an empty paste and the clipboard holds an image, return a
/// path payload (re-wrapped when `text` used DECSET 2004 delimiters).
///
/// Paste-event call sites (windowed host clipboard, nested `Event::Paste`)
/// may pass a blank payload with no wrappers. Mux-attach typing must use
/// [`expand_empty_bracketed_paste`] so a space or Enter is not rewritten.
#[must_use]
pub fn expand_empty_paste(text: &str, child_pid: Option<u32>) -> Option<String> {
    expand_if_empty(text, child_pid, false)
}

/// Like [`expand_empty_paste`], but only when the chunk includes bracketed
/// paste markers. Attach stdin is keystrokes; a blank chunk is not a paste.
#[must_use]
pub fn expand_empty_bracketed_paste(text: &str, child_pid: Option<u32>) -> Option<String> {
    expand_if_empty(text, child_pid, true)
}

fn expand_if_empty(text: &str, child_pid: Option<u32>, require_markers: bool) -> Option<String> {
    if require_markers {
        if !is_empty_bracketed_paste(text) {
            return None;
        }
    } else if !is_empty_bracketed_or_blank(text) {
        return None;
    }
    let path = clipboard_image_to_png()?;
    let agent = detect_inject_agent(child_pid, None);
    let body = paste_reference(&path, agent);
    if text.contains(PASTE_START) || text.contains(PASTE_END) {
        Some(format!("{PASTE_START}{body}{PASTE_END}"))
    } else {
        Some(body)
    }
}

/// Read `image/png` (or arboard RGBA) from the system clipboard and save.
#[must_use]
pub fn clipboard_image_to_png() -> Option<PathBuf> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard_image_to_png_with(&mut clipboard)
}

fn strip_bracketed(text: &str) -> String {
    text.replace(PASTE_START, "").replace(PASTE_END, "")
}

fn prune_old(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("png") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    if files.len() <= MAX_KEEP {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    let drop = files.len() - MAX_KEEP;
    for (_, path) in files.into_iter().take(drop) {
        let _ = fs::remove_file(path);
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn encode_rgba_roundtrip_one_pixel() {
        let png = encode_rgba_png(1, 1, &[255, 0, 0, 255]).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (1, 1));
        assert_eq!(&buf[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn write_paste_png_in_creates_readable_file() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("prism-paste-test-{stamp}"));
        let path = write_paste_png_in(&dir, 1, 1, &[0, 255, 0, 255]).unwrap();
        assert!(path.starts_with(&dir));
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("png"));
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > 8);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_paste_png_in_keeps_last_eight_files() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("prism-paste-retention-test-{stamp}"));
        for _ in 0..10 {
            write_paste_png_in(&dir, 1, 1, &[0, 0, 255, 255]).unwrap();
        }
        let count = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("png"))
            .count();
        assert_eq!(count, MAX_KEEP);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_bracketed_paste_is_detected() {
        assert!(is_empty_bracketed_or_blank(""));
        assert!(is_empty_bracketed_or_blank("  \n"));
        assert!(is_empty_bracketed_or_blank("\x1b[200~\x1b[201~"));
        assert!(is_empty_bracketed_or_blank("\x1b[200~ \x1b[201~"));
        assert!(!is_empty_bracketed_or_blank("hi"));
        assert!(!is_empty_bracketed_or_blank("\x1b[200~hi\x1b[201~"));
    }

    #[test]
    fn typing_whitespace_is_not_an_empty_bracketed_paste() {
        assert!(!is_empty_bracketed_paste(""));
        assert!(!is_empty_bracketed_paste(" "));
        assert!(!is_empty_bracketed_paste("\r"));
        assert!(!is_empty_bracketed_paste("\n"));
        assert!(!is_empty_bracketed_paste("  \n"));
        assert!(is_empty_bracketed_paste("\x1b[200~\x1b[201~"));
        assert!(is_empty_bracketed_paste("\x1b[200~ \x1b[201~"));
        assert!(!is_empty_bracketed_paste("\x1b[200~hi\x1b[201~"));
        assert_eq!(expand_empty_bracketed_paste(" ", None), None);
        assert_eq!(expand_empty_bracketed_paste("\r", None), None);
        assert_eq!(expand_empty_bracketed_paste("hello", None), None);
    }

    #[test]
    fn paste_reference_is_at_path_for_cursor() {
        let path = Path::new("/run/user/1000/prism-paste/ab.png");
        assert_eq!(
            paste_reference(path, InjectAgent::Cursor),
            "@/run/user/1000/prism-paste/ab.png"
        );
        assert_eq!(
            paste_reference(path, InjectAgent::Grok),
            "/run/user/1000/prism-paste/ab.png"
        );
    }

    #[test]
    fn parse_file_uri_list_decodes_image_path() {
        assert_eq!(
            parse_file_uri_list("file:///tmp/white%20space.png").as_deref(),
            Some([PathBuf::from("/tmp/white space.png")].as_slice())
        );
        assert_eq!(
            parse_file_uri_list("file:///tmp/a.png\nfile:///tmp/b.png")
                .expect("two file URIs")
                .len(),
            2
        );
        assert_eq!(parse_file_uri_list("https://example.test/image.png"), None);
        assert_eq!(parse_file_uri_list("file:///tmp/bad%ZZ.png"), None);
    }

    #[test]
    fn image_file_path_requires_one_image_file() {
        assert_eq!(
            single_image_path(vec![PathBuf::from("/tmp/shot.PNG")]),
            Some(PathBuf::from("/tmp/shot.PNG"))
        );
        assert_eq!(
            single_image_path(vec![PathBuf::from("/tmp/notes.txt")]),
            None
        );
        assert_eq!(
            single_image_path(vec![
                PathBuf::from("/tmp/a.png"),
                PathBuf::from("/tmp/b.png")
            ]),
            None
        );
    }

    #[test]
    fn expand_skips_non_empty_text() {
        assert_eq!(expand_empty_paste("hello", None), None);
        assert_eq!(expand_empty_paste("\x1b[200~hello\x1b[201~", None), None);
    }
}
