// SPDX-License-Identifier: MIT
//! Authoritative component definitions. Native transport and storage stay private.

pub const API_NAME: &str = env!("PMUX_PLUGIN_API_NAME");
pub const API_VERSION: &str = env!("PMUX_PLUGIN_API_VERSION");
/// Fits the activity monitor's wide Graphite view (132x30 at the default
/// 18x41 px macOS cell needs about 5.1 MiB), with room to spare.
pub const MAX_FRAME_BYTES: usize = 6 * 1024 * 1024;
pub const MAX_VIEW_CELLS: usize = 60_000;
/// The same wide Graphite view draws 213 tiles.
pub const MAX_TILES: usize = 256;
/// The frame budget hosts granted before `MAX_FRAME_BYTES` was raised. Those
/// hosts accept at most `LEGACY_MAX_TILES` tiles.
pub const LEGACY_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const LEGACY_MAX_TILES: usize = 32;
pub const MAX_TILE_DIMENSION: u32 = 4096;
pub const MAX_HOST_IMPORTS_PER_CALL: u16 = 64;
pub const MAX_ACTIVITY_READS_PER_SECOND: u16 = 64;

/// The tiles a host accepts, read from the frame budget it grants: a host that
/// grants more than `LEGACY_FRAME_BYTES` accepts `MAX_TILES`.
pub const fn tile_limit(frame_bytes: u32) -> usize {
    if frame_bytes as usize > LEGACY_FRAME_BYTES {
        MAX_TILES
    } else {
        LEGACY_MAX_TILES
    }
}

pub mod monitor;

pub mod modifiers {
    pub const SHIFT: u8 = 1;
    pub const ALT: u8 = 2;
    pub const CONTROL: u8 = 4;
    pub const SUPER: u8 = 8;
    pub const HYPER: u8 = 16;
    pub const META: u8 = 32;
    pub const CAPS_LOCK: u8 = 64;
    pub const NUM_LOCK: u8 = 128;
}
#[cfg(feature = "guest")]
pub mod activity;
include!(concat!(env!("OUT_DIR"), "/contracts.rs"));

#[cfg(feature = "host")]
mod frame;
#[cfg(feature = "host")]
pub use frame::{validate_viewport, FrameError, ValidatedFrame};

#[cfg(feature = "host")]
pub mod host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
        ownership: Owning,
        additional_derives: [serde::Serialize, serde::Deserialize, PartialEq],
    });

    impl Default for pmux::view::frame::Preferences {
        fn default() -> Self {
            Self {
                snapshot: false,
                motion: true,
                palette: pmux::view::frame::Palette::Auto,
                dark_background: None,
            }
        }
    }
}

#[cfg(feature = "guest")]
pub mod guest {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        ownership: Owning,
        additional_derives: [serde::Serialize, serde::Deserialize, PartialEq],
        generate_all,
        pub_export_macro: true,
        export_macro_name: "export_plugin",
        default_bindings_module: "pmux_plugin_api::guest",
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_that_grants_the_legacy_budget_gets_the_legacy_tile_count() {
        assert_eq!(tile_limit(LEGACY_FRAME_BYTES as u32), LEGACY_MAX_TILES);
        assert_eq!(tile_limit(LEGACY_FRAME_BYTES as u32 + 1), MAX_TILES);
        assert_eq!(tile_limit(MAX_FRAME_BYTES as u32), MAX_TILES);
    }
}
