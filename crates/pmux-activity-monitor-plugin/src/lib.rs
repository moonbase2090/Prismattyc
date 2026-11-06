// SPDX-License-Identifier: MIT
//! Activity layout and state without terminal or transport ownership.

pub mod app;
#[cfg(any(target_arch = "wasm32", test))]
mod capture;
mod graph;
#[cfg(target_arch = "wasm32")]
mod guest;
pub mod model;
pub mod ui;
