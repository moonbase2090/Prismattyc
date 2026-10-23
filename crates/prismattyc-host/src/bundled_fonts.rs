// SPDX-License-Identifier: MIT
//! Optional terminal faces. An absent choice keeps the existing discovery chain.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BundledFont {
    pub id: &'static str,
    pub name: &'static str,
    pub bytes: &'static [u8],
}

pub(crate) const FONTS: &[BundledFont] = &[
    BundledFont {
        id: "bundled:ibm-plex-mono",
        name: "IBM Plex Mono",
        bytes: include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf"),
    },
    BundledFont {
        id: "bundled:commit-mono",
        name: "Commit Mono",
        bytes: include_bytes!("../assets/fonts/CommitMonoV143-400Regular.otf"),
    },
];

pub(crate) fn find(id: &str) -> Option<BundledFont> {
    FONTS.iter().copied().find(|font| font.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_faces_are_named_monospace_outlines() {
        for (id, expected_family) in [
            ("bundled:ibm-plex-mono", "IBM Plex Mono"),
            ("bundled:commit-mono", "CommitMonoV143"),
        ] {
            let bundled = find(id).expect("selectable bundled face");
            let face = ttf_parser::Face::parse(bundled.bytes, 0).expect("valid OpenType font");
            let family = face
                .names()
                .into_iter()
                .filter(|name| name.name_id == ttf_parser::name_id::FAMILY)
                .find_map(|name| name.to_string())
                .expect("family name");
            assert_eq!(family, expected_family, "{id}");
            let i = face.glyph_index('i').expect("lowercase i");
            let w = face.glyph_index('W').expect("uppercase W");
            assert_eq!(face.glyph_hor_advance(i), face.glyph_hor_advance(w));
            let font = fontdue::Font::from_bytes(bundled.bytes, fontdue::FontSettings::default())
                .expect("load with production rasterizer");
            let (metrics, pixels) = font.rasterize('W', 15.0);
            assert!(metrics.width > 0 && metrics.height > 0);
            assert!(pixels.iter().any(|&pixel| pixel > 0));
        }
        assert!(find("bundled:not-a-font").is_none());
    }
}
