// SPDX-License-Identifier: MIT
//! Authoritative component definitions. Native transport and storage stay private.

pub const API_NAME: &str = env!("PMUX_PLUGIN_API_NAME");
pub const API_VERSION: &str = env!("PMUX_PLUGIN_API_VERSION");
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_VIEW_CELLS: usize = 60_000;
pub const MAX_TILES: usize = 32;
pub const MAX_TILE_DIMENSION: u32 = 4096;
pub const MAX_HOST_IMPORTS_PER_CALL: u16 = 64;
pub const MAX_ACTIVITY_READS_PER_SECOND: u16 = 64;

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
