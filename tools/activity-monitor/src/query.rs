// SPDX-License-Identifier: MIT
//! `--seats`, `--recent` and `--tail`: thin wrappers over the shared bounded
//! queries in `prismattyc_mux::activity_query`, which `pmux activity` also uses.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use prismattyc_mux::activity_query::{self, TailLimits};
use prismattyc_mux::mailbox::AgentId;
use serde_json::Value;

use super::{read_page, POLL_INTERVAL};

fn history(socket: &Path) -> Result<activity_query::Retained> {
    activity_query::retained(|cursor| read_page(socket, cursor))
        .map_err(|_| anyhow::anyhow!("cannot read a consistent retained activity history; retry"))
}

pub(super) fn seats(socket: &Path) -> Result<Value> {
    Ok(activity_query::observed_seats(&history(socket)?))
}

pub(super) fn recent(socket: &Path, seat: &AgentId) -> Result<Value> {
    Ok(activity_query::recent_events(&history(socket)?, seat, None))
}

fn line(out: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    out.flush()
}

pub(super) fn tail(socket: &Path, seat: Option<&AgentId>) -> Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stopped);
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))?;
    let mut out = io::stdout().lock();
    activity_query::tail(
        |cursor| read_page(socket, cursor),
        None,
        seat,
        TailLimits::default(),
        POLL_INTERVAL,
        &stopped,
        |value| line(&mut out, value),
    )
}
