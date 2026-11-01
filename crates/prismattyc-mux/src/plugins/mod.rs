// SPDX-License-Identifier: MIT
//! Daemon-owned plug-in packages, selections and private worker processes.

mod catalog;
mod cli;
mod client;
mod endpoint;
mod input;
mod manager;
mod manifest;
mod package;
mod paint;
mod presenter;
mod protocol;
mod remote;
mod schema;
mod wire;
mod worker;

pub use catalog::CatalogEntry;
pub use cli::run as run_cli;
pub use client::{decode_frame, Client};
pub use endpoint::{Endpoint, EndpointSource};
pub(crate) use manager::Service;
pub use manifest::{Contribution, Manifest};
pub use package::{pack, Installed};
pub use protocol::{
    AdminAction, Observed, Reply, Request, RetryReason, Status, ToolExport, ViewEndpoint,
};
pub(crate) use wire::error_message;

pub(crate) fn connection_id() -> anyhow::Result<String> {
    package::nonce()
}

/// Private subprocess entry point. It must precede normal daemon startup.
pub const INTERNAL_WORKER_ARG: &str = "--internal-plugin-worker";
pub use remote::{run_worker as run_endpoint_worker, INTERNAL_ENDPOINT_ARG};

pub fn run_internal_worker() -> anyhow::Result<()> {
    worker::run()
}
