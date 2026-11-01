// SPDX-License-Identifier: MIT
//! Authoritative component definitions. Native transport and storage stay private.

pub const API_NAME: &str = env!("PMUX_PLUGIN_API_NAME");
pub const API_VERSION: &str = env!("PMUX_PLUGIN_API_VERSION");
include!(concat!(env!("OUT_DIR"), "/contracts.rs"));

#[cfg(feature = "host")]
mod frame;
#[cfg(feature = "host")]
pub use frame::{validate_viewport, FrameError, ValidatedFrame, MAX_FRAME_BYTES, MAX_VIEW_CELLS};

#[cfg(feature = "host")]
pub mod host {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
        ownership: Owning,
        additional_derives: [serde::Serialize, serde::Deserialize],
    });
}

#[cfg(feature = "guest")]
pub mod guest {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        ownership: Owning,
        additional_derives: [serde::Serialize, serde::Deserialize],
        generate_all,
        pub_export_macro: true,
        export_macro_name: "export_plugin",
        default_bindings_module: "pmux_plugin_api::guest",
    });
}
