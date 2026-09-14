//! System font discovery for glyphs the bundled faces omit.
//!
//! Ghostty reference (`src/font/discovery.zig`):
//! - Linux: fontconfig charset match (`FcFontMatch` + `FC_CHARSET`)
//! - macOS: Core Text cascade / `CTFontCreateForString` for Han
//! - Never return LastResort tofu
//! - Do not bundle CJK or color-emoji; use whatever the OS has
//!
//! Prismattyc keeps a short well-known path list (fast, no FFI) and on Linux
//! asks fontconfig for any other installed script.

use std::path::{Path, PathBuf};

/// A system face that covers one codepoint. `index` is the TTC/OTC face.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoveringFace {
    pub path: PathBuf,
    pub index: u32,
}

/// Find a system font file that maps `ch`. Cached by the caller.
pub fn covering_face(ch: char) -> Option<CoveringFace> {
    if let Some(face) = from_well_known_paths(ch) {
        return Some(face);
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(face) = fontconfig_match(ch) {
            if !is_last_resort(&face.path) {
                return Some(face);
            }
        }
    }
    None
}

fn from_well_known_paths(ch: char) -> Option<CoveringFace> {
    for raw in well_known_paths() {
        let path = Path::new(raw);
        if !path.is_file() {
            continue;
        }
        if is_last_resort(path) {
            continue;
        }
        if let Some(index) = face_index_covering(path, ch) {
            return Some(CoveringFace {
                path: path.to_path_buf(),
                index,
            });
        }
    }
    None
}

fn well_known_paths() -> &'static [&'static str] {
    &[
        // Linux CJK (Noto / Source Han / WQY). Not bundled.
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/noto-cjk/NotoSansCJKjp-Regular.otf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/TTF/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/adobe-source-han-sans/SourceHanSansCN-Regular.otf",
        "/usr/share/fonts/source-han-sans-cn/SourceHanSansCN-Regular.otf",
        "/usr/share/fonts/wenquanyi/wqy-zenhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/truetype/arphic/uming.ttc",
        "/usr/share/fonts/OTF/NotoSansCJK-Regular.ttc",
        // macOS CJK. Locale-aware Han is Core Text; these cover SC/JP/KR.
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Hiragino Sans W3.ttc",
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
    ]
}

fn is_last_resort(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("LastResort.otf") || n.contains("LastResort"))
}

/// Probe TTC faces until cmap contains `ch`. Single-face files use index 0.
fn face_index_covering(path: &Path, ch: char) -> Option<u32> {
    let data = std::fs::read(path).ok()?;
    for index in 0..8u32 {
        let Ok(face) = ttf_parser::Face::parse(&data, index) else {
            break;
        };
        if face.glyph_index(ch).is_some() {
            return Some(index);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn fontconfig_match(ch: char) -> Option<CoveringFace> {
    use std::ffi::CStr;
    use std::os::raw::c_char;
    use std::ptr;

    use fontconfig_sys::constants::{FC_CHARSET, FC_FILE};
    use fontconfig_sys::statics::LIB_RESULT;
    use fontconfig_sys::{FcChar8, FcMatchPattern, FcResult};

    let lib = LIB_RESULT.as_ref().ok()?;
    unsafe {
        if (lib.FcInit)() == 0 {
            return None;
        }
        let pat = (lib.FcPatternCreate)();
        if pat.is_null() {
            return None;
        }
        let cs = (lib.FcCharSetCreate)();
        if cs.is_null() {
            (lib.FcPatternDestroy)(pat);
            return None;
        }
        (lib.FcCharSetAddChar)(cs, u32::from(ch));
        (lib.FcPatternAddCharSet)(pat, FC_CHARSET.as_ptr().cast::<c_char>(), cs);
        (lib.FcConfigSubstitute)(ptr::null_mut(), pat, FcMatchPattern);
        (lib.FcDefaultSubstitute)(pat);
        let mut result: FcResult = 0;
        // trim=true: only fonts whose charset includes the codepoint
        // (Ghostty uses fontSort; a bare FcFontMatch can return a
        // closest-but-uncovered face such as Noto Sans Regular).
        let set = (lib.FcFontSort)(ptr::null_mut(), pat, 1, ptr::null_mut(), &mut result);
        (lib.FcCharSetDestroy)(cs);
        (lib.FcPatternDestroy)(pat);
        if set.is_null() {
            return None;
        }
        let n = (*set).nfont.min(16);
        let mut found = None;
        for i in 0..n {
            let font_pat = *(*set).fonts.add(i as usize);
            if font_pat.is_null() {
                continue;
            }
            let mut file_ptr: *mut FcChar8 = ptr::null_mut();
            let got_file = (lib.FcPatternGetString)(
                font_pat,
                FC_FILE.as_ptr().cast::<c_char>(),
                0,
                &mut file_ptr,
            );
            if got_file != 0 || file_ptr.is_null() {
                continue;
            }
            let Ok(path_str) = CStr::from_ptr(file_ptr.cast::<c_char>()).to_str() else {
                continue;
            };
            let path = PathBuf::from(path_str);
            if !path.is_file() || is_last_resort(&path) {
                continue;
            }
            if let Some(index) = face_index_covering(&path, ch) {
                found = Some(CoveringFace { path, index });
                break;
            }
        }
        (lib.FcFontSetDestroy)(set);
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_resort_is_rejected() {
        assert!(is_last_resort(Path::new(
            "/System/Library/Fonts/LastResort.otf"
        )));
        assert!(!is_last_resort(Path::new(
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"
        )));
    }

    #[test]
    fn covering_face_skips_when_no_cjk_installed() {
        // Must not panic. Returns Some only if the host has a CJK face.
        let _ = covering_face('中');
    }
}
