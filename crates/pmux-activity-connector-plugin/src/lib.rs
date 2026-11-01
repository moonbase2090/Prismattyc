// SPDX-License-Identifier: MIT
//! Activity queries without native transports, process launching or credentials.

#[cfg(target_arch = "wasm32")]
mod guest;
pub mod query;
pub mod service;
