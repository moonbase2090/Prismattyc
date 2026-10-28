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

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

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

/// Sorted, non-overlapping inclusive codepoint ranges, one list per TTC face.
type Coverage = Vec<Vec<(u32, u32)>>;

/// Per-path coverage index. Each font file is read and parsed once per process;
/// later lookups are a binary search over its ranges, so a screen of new
/// characters does not re-read the file for every codepoint.
static COVERAGE: OnceLock<Mutex<HashMap<PathBuf, Arc<Coverage>>>> = OnceLock::new();

/// Test-only log of every font file read, to prove reads happen once per path.
#[cfg(test)]
static FONT_READS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Probe TTC faces until cmap contains `ch`. Single-face files use index 0.
fn face_index_covering(path: &Path, ch: char) -> Option<u32> {
    let coverage = coverage_for(path);
    let cp = u32::from(ch);
    coverage
        .iter()
        .position(|ranges| range_contains(ranges, cp))
        .map(|index| index as u32)
}

fn coverage_for(path: &Path) -> Arc<Coverage> {
    let cache = COVERAGE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = match cache.lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(coverage) = cache.get(path) {
        return coverage.clone();
    }
    // An unreadable file caches as empty coverage, so it is not retried per character.
    let coverage = Arc::new(
        read_font(path)
            .map(|data| build_coverage(&data))
            .unwrap_or_default(),
    );
    cache.insert(path.to_path_buf(), coverage.clone());
    coverage
}

fn read_font(path: &Path) -> Option<Vec<u8>> {
    #[cfg(test)]
    if let Ok(mut reads) = FONT_READS.lock() {
        reads.push(path.to_path_buf());
    }
    std::fs::read(path).ok()
}

fn build_coverage(data: &[u8]) -> Coverage {
    let mut faces = Vec::new();
    for index in 0..8u32 {
        let Ok(face) = ttf_parser::Face::parse(data, index) else {
            break;
        };
        let mut codepoints = Vec::new();
        if let Some(cmap) = face.tables().cmap {
            for subtable in cmap.subtables {
                if !subtable.is_unicode() {
                    continue;
                }
                subtable.codepoints(|cp| {
                    if subtable.glyph_index(cp).is_some() {
                        codepoints.push(cp);
                    }
                });
            }
        }
        faces.push(to_ranges(codepoints));
    }
    faces
}

fn to_ranges(mut codepoints: Vec<u32>) -> Vec<(u32, u32)> {
    codepoints.sort_unstable();
    codepoints.dedup();
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for cp in codepoints {
        match ranges.last_mut() {
            Some(last) if last.1 + 1 == cp => last.1 = cp,
            _ => ranges.push((cp, cp)),
        }
    }
    ranges
}

fn range_contains(ranges: &[(u32, u32)], cp: u32) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                Ordering::Less
            } else if lo > cp {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
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
    fn covering_face_finds_installed_cjk_glyph() {
        let face = covering_face('中').expect("test environment must provide a CJK face");
        assert!(
            face.path.is_file(),
            "font file is missing: {}",
            face.path.display()
        );
        assert!(!is_last_resort(&face.path));
        assert_eq!(face_index_covering(&face.path, '中'), Some(face.index));
        assert_eq!(covering_face('中'), Some(face));
    }

    #[test]
    fn new_cjk_characters_do_not_reread_the_font_file() {
        let face = covering_face('中').expect("test environment must provide a CJK face");
        // A screen of new CJK text: many distinct codepoints, one lookup each.
        for ch in (0x4e00u32..0x4f00).filter_map(char::from_u32) {
            let _ = covering_face(ch);
        }
        let reads = FONT_READS
            .lock()
            .unwrap()
            .iter()
            .filter(|path| **path == face.path)
            .count();
        // The cache is process-wide and parallel tests share it, so the file is
        // read at most once no matter how many characters are looked up.
        assert!(
            reads <= 1,
            "font file was read {reads} times for 256 characters"
        );
    }

    #[test]
    fn coverage_index_matches_direct_cmap_lookup() {
        let face = covering_face('中').expect("test environment must provide a CJK face");
        let data = std::fs::read(&face.path).expect("font file is readable");
        for ch in (0x20u32..0x7f)
            .chain(0x3000..0x3100)
            .chain(0x4e00..0x4e80)
            .filter_map(char::from_u32)
        {
            // First face index whose cmap maps `ch`, as the direct parse sees it.
            let direct = (0..8u32).find(|&index| {
                ttf_parser::Face::parse(&data, index)
                    .map(|parsed| parsed.glyph_index(ch).is_some())
                    .unwrap_or(false)
            });
            assert_eq!(
                face_index_covering(&face.path, ch),
                direct,
                "coverage mismatch for U+{:04X}",
                ch as u32
            );
        }
    }
}
