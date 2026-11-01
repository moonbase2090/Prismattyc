// SPDX-License-Identifier: MIT
//! Daemon-owned plug-in packages, selections and private worker processes.

mod bundle;
mod catalog;
mod cli;
mod client;
mod endpoint;
mod endpoint_import;
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

pub use bundle::{
    fetch_bundle, pin_candidate_bundle, verify_bundle, BuildReceipt, BundlePin, BundleSource,
    PackageBuild,
};
pub use catalog::CatalogEntry;
pub use cli::{admin as admin_request, run as run_cli};
pub use client::{decode_frame, Client, ConnectionCloser};
pub use endpoint::{Endpoint, EndpointDefinition, EndpointSource};
pub use manager::inspect_offline;
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

/// Report the physical executable's embedded resources before managed forwarding.
/// This read-only build check never connects to or starts a daemon.
pub fn print_bundled_identity() -> anyhow::Result<bool> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--plugin-bundle")) {
        return Ok(false);
    }
    anyhow::ensure!(args.next().is_none(), "usage: --plugin-bundle");
    println!(
        "{}",
        serde_json::to_string_pretty(&catalog::bundled_identity()?)?
    );
    Ok(true)
}
