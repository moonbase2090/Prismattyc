// SPDX-License-Identifier: MIT
//! Daemon-owned plug-in packages, selections and private worker processes.

mod catalog;
mod cli;
mod client;
mod manager;
mod manifest;
mod package;
mod protocol;
mod schema;
mod wire;
mod worker;

pub use catalog::CatalogEntry;
pub use cli::run as run_cli;
pub use client::{decode_frame, Client};
pub(crate) use manager::Service;
pub use manifest::{Contribution, Manifest};
pub use package::{pack, Installed};
pub use protocol::{AdminAction, Observed, Reply, Request, Status, ToolExport};

pub(crate) fn connection_id() -> anyhow::Result<String> {
    package::nonce()
}

/// Private subprocess entry point. It must precede normal daemon startup.
pub const INTERNAL_WORKER_ARG: &str = "--internal-plugin-worker";

pub fn run_internal_worker() -> anyhow::Result<()> {
    worker::run()
}
