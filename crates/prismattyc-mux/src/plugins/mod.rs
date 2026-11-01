// SPDX-License-Identifier: MIT
//! Daemon-owned plug-in packages, selections and private worker processes.

mod wire;
mod worker;

/// Private subprocess entry point. It must precede normal daemon startup.
pub const INTERNAL_WORKER_ARG: &str = "--internal-plugin-worker";

pub fn run_internal_worker() -> anyhow::Result<()> {
    worker::run()
}
