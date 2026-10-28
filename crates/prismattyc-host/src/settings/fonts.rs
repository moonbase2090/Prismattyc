// SPDX-License-Identifier: MIT
//! Font choices contain portable bundled identities or installed face paths.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub(super) struct Choice {
    pub label: String,
    pub identity: Option<String>,
}

pub(super) fn bundled() -> Vec<Choice> {
    std::iter::once(Choice {
        label: "Default font chain".into(),
        identity: None,
    })
    .chain(crate::bundled_fonts::FONTS.iter().map(|font| Choice {
        label: format!("{} (bundled)", font.name),
        identity: Some(font.id.to_string()),
    }))
    .collect()
}

pub(super) fn installed() -> Vec<Choice> {
    let mut pending = directories();
    let mut paths = std::collections::BTreeSet::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && entry
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        ["ttf", "otf", "ttc"]
                            .iter()
                            .any(|known| ext.eq_ignore_ascii_case(known))
                    })
            {
                paths.insert(entry.path());
            }
        }
    }
    let mut faces = std::collections::BTreeMap::new();
    for path in paths {
        if let Some(label) = face_label(&path) {
            faces.entry(label).or_insert(path);
        }
    }
    faces
        .into_iter()
        .map(|(label, path)| Choice {
            label,
            identity: Some(path.to_string_lossy().into_owned()),
        })
        .collect()
}

fn face_label(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&bytes, 0).ok()?;
    // Include patched monos whose symbol glyphs make fixed-pitch metadata false.
    let mut widths = " iW0m"
        .chars()
        .map(|ch| face.glyph_hor_advance(face.glyph_index(ch)?));
    let width = widths.next()??;
    if width == 0 || !widths.all(|advance| advance == Some(width)) {
        return None;
    }
    let name = |id| {
        face.names()
            .into_iter()
            .filter(|name| name.name_id == id)
            .find_map(|name| name.to_string())
    };
    let family = name(ttf_parser::name_id::FAMILY)?;
    let style = name(ttf_parser::name_id::SUBFAMILY).unwrap_or_default();
    Some(if style.is_empty() {
        family
    } else {
        format!("{family} {style}")
    })
}

fn directories() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    let mut roots: Vec<PathBuf> = ["/System/Library/Fonts", "/Library/Fonts"]
        .into_iter()
        .map(Into::into)
        .collect();
    #[cfg(target_os = "linux")]
    let mut roots: Vec<PathBuf> = ["/usr/share/fonts", "/usr/local/share/fonts"]
        .into_iter()
        .map(Into::into)
        .collect();
    #[cfg(windows)]
    let mut roots: Vec<PathBuf> = std::env::var_os("WINDIR")
        .into_iter()
        .map(|root| PathBuf::from(root).join("Fonts"))
        .collect();
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = prismattyc_mux::platform::home_dir() {
        #[cfg(target_os = "macos")]
        roots.push(PathBuf::from(home).join("Library/Fonts"));
        #[cfg(target_os = "linux")]
        roots.extend([
            PathBuf::from(&home).join(".fonts"),
            PathBuf::from(home).join(".local/share/fonts"),
        ]);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = home;
    }
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Microsoft/Windows/Fonts"));
    }
    roots
}
