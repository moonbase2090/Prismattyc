// SPDX-License-Identifier: MIT

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Cell rendering choices shared by the monitor launcher and component.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "host", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GraphStyle {
    #[default]
    Braille,
    Block,
    Ascii,
}

impl GraphStyle {
    /// Existing named views carry the choice without changing the component ABI.
    pub const fn view(self) -> &'static str {
        match self {
            Self::Braille => "main",
            Self::Block => "block",
            Self::Ascii => "ascii",
        }
    }

    pub fn from_view(view: &str) -> Result<Self, &'static str> {
        match view {
            "main" => Ok(Self::Braille),
            "block" => Ok(Self::Block),
            "ascii" => Ok(Self::Ascii),
            _ => Err("unknown activity monitor view"),
        }
    }

    /// Unknown font coverage keeps the requested style. Only a known text-only
    /// terminal selects the ASCII fallback.
    pub const fn for_terminal(self, ascii_only: bool) -> Self {
        if ascii_only {
            Self::Ascii
        } else {
            self
        }
    }
}

impl FromStr for GraphStyle {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "braille" => Ok(Self::Braille),
            "block" => Ok(Self::Block),
            "ascii" => Ok(Self::Ascii),
            _ => Err("graph style requires braille, block or ascii"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_explicit_styles_select_declared_views() {
        assert_eq!(GraphStyle::default(), GraphStyle::Braille);
        for (value, style, view) in [
            ("braille", GraphStyle::Braille, "main"),
            ("block", GraphStyle::Block, "block"),
            ("ascii", GraphStyle::Ascii, "ascii"),
        ] {
            assert_eq!(value.parse::<GraphStyle>(), Ok(style));
            assert_eq!(
                serde_json::from_str::<GraphStyle>(&format!("\"{value}\"")).unwrap(),
                style
            );
            assert_eq!(style.view(), view);
            assert_eq!(GraphStyle::from_view(view), Ok(style));
        }
        assert!("pixels".parse::<GraphStyle>().is_err());
        assert!(GraphStyle::from_view("unknown").is_err());
    }

    #[test]
    fn only_known_ascii_terminals_change_the_requested_style() {
        assert_eq!(GraphStyle::Braille.for_terminal(false), GraphStyle::Braille);
        assert_eq!(GraphStyle::Braille.for_terminal(true), GraphStyle::Ascii);
        assert_eq!(GraphStyle::Block.for_terminal(false), GraphStyle::Block);
        assert_eq!(GraphStyle::Block.for_terminal(true), GraphStyle::Ascii);
        assert_eq!(GraphStyle::Ascii.for_terminal(false), GraphStyle::Ascii);
    }
}
