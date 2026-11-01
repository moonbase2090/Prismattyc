// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Result};
use prismattyc_mux::activity::{
    ActivityCursor, ActivityKind, ActivityPage, ActivityRecord, ActivitySource,
};
use prismattyc_mux::mailbox::AgentId;
use serde::Serialize;
use serde_json::{json, Value};

use super::{read_page, POLL_INTERVAL};

const MAX_RETAINED: usize = 512;
const MAX_QUERY_PAGES: usize = 2;

struct Retained {
    epoch: String,
    oldest_sequence: u64,
    cursor: ActivityCursor,
    records: Vec<ActivityRecord>,
}

fn retained(
    mut read: impl FnMut(Option<ActivityCursor>) -> Result<ActivityPage>,
) -> Result<Retained> {
    let mut page = read(None)?;
    let epoch = page.epoch.clone();
    let oldest_sequence = page.oldest_sequence;
    let watermark = page.current_sequence;
    let mut records = Vec::new();
    let mut prior_sequence = None;
    for page_index in 0..MAX_QUERY_PAGES {
        if page.epoch != epoch || page.reset.is_some() {
            bail!("activity feed changed during the bounded query; retry");
        }
        if prior_sequence.is_some_and(|sequence| page.cursor.sequence <= sequence) {
            bail!("activity feed cursor did not advance");
        }
        records.extend(
            page.records
                .into_iter()
                .filter(|record| record.sequence <= watermark),
        );
        if records.len() > MAX_RETAINED {
            bail!("activity feed exceeded the bounded query");
        }
        if page.cursor.sequence >= watermark {
            return Ok(Retained {
                epoch: epoch.clone(),
                oldest_sequence,
                cursor: ActivityCursor {
                    epoch,
                    sequence: watermark,
                },
                records,
            });
        }
        if !page.has_more {
            bail!("activity feed ended before the query watermark");
        }
        if page_index + 1 == MAX_QUERY_PAGES {
            bail!("activity feed exceeded the bounded query page limit");
        }
        prior_sequence = Some(page.cursor.sequence);
        page = read(Some(page.cursor))?;
    }
    bail!("activity feed exceeded the bounded query page limit")
}

#[derive(Serialize)]
struct ObservedSeat<'a> {
    seat: &'a AgentId,
    first_retained_sequence: u64,
    last_sequence: u64,
    last_received_at_ms: u64,
    last_source: ActivitySource,
    last_kind: ActivityKind,
    retained_events: usize,
}

fn base(history: &Retained) -> Value {
    json!({
        "schema_version": 1,
        "retained_history_only": true,
        "epoch": history.epoch,
        "oldest_sequence": history.oldest_sequence,
        "observed_through_sequence": history.cursor.sequence,
        "cursor": history.cursor
    })
}

fn observed_seats(history: &Retained) -> Value {
    let mut seats = BTreeMap::new();
    for record in &history.records {
        let entry = seats.entry(&record.event.seat).or_insert(ObservedSeat {
            seat: &record.event.seat,
            first_retained_sequence: record.sequence,
            last_sequence: record.sequence,
            last_received_at_ms: record.received_at_ms,
            last_source: record.event.source,
            last_kind: record.event.kind,
            retained_events: 0,
        });
        entry.last_sequence = record.sequence;
        entry.last_received_at_ms = record.received_at_ms;
        entry.last_source = record.event.source;
        entry.last_kind = record.event.kind;
        entry.retained_events += 1;
    }
    let mut result = base(history);
    result["seats"] = json!(seats.into_values().collect::<Vec<_>>());
    result
}

pub(super) fn seats(socket: &Path) -> Result<Value> {
    let history = retained(|cursor| read_page(socket, cursor)).map_err(|_| {
        anyhow::anyhow!("cannot read a consistent retained activity history; retry")
    })?;
    Ok(observed_seats(&history))
}

fn recent_events(history: &Retained, seat: &AgentId) -> Value {
    let records = history
        .records
        .iter()
        .filter(|record| &record.event.seat == seat)
        .collect::<Vec<_>>();
    let mut result = base(history);
    result["seat"] = json!(seat);
    result["records"] = json!(records);
    result
}

pub(super) fn recent(socket: &Path, seat: &AgentId) -> Result<Value> {
    let history = retained(|cursor| read_page(socket, cursor)).map_err(|_| {
        anyhow::anyhow!("cannot read a consistent retained activity history; retry")
    })?;
    Ok(recent_events(&history, seat))
}

fn filtered_page(mut page: ActivityPage, seat: Option<&AgentId>) -> ActivityPage {
    if let Some(seat) = seat {
        page.records.retain(|record| &record.event.seat == seat);
    }
    page
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
    let mut cursor = None;
    let mut disconnected = false;
    let mut out = io::stdout().lock();
    while !stopped.load(Ordering::Relaxed) {
        let message = match read_page(socket, cursor.clone()) {
            Ok(page) => {
                let changed =
                    cursor.as_ref() != Some(&page.cursor) || page.reset.is_some() || disconnected;
                cursor = Some(page.cursor.clone());
                disconnected = false;
                changed.then(|| {
                    json!({
                        "type": "activity",
                        "seat_filter": seat,
                        "page": filtered_page(page, seat)
                    })
                })
            }
            Err(_) if !disconnected => {
                disconnected = true;
                Some(json!({"type": "disconnected", "retrying": true, "cursor": cursor}))
            }
            Err(_) => None,
        };
        if let Some(message) = message {
            if let Err(error) = line(&mut out, &message) {
                if error.kind() == io::ErrorKind::BrokenPipe || stopped.load(Ordering::Relaxed) {
                    return Ok(());
                }
                return Err(error.into());
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{page, record};

    #[test]
    fn seat_summary_counts_events_and_uses_the_latest_kind() -> Result<()> {
        let history = retained(|_| {
            page(
                vec![
                    record("worker-a", 1, "working")?,
                    record("worker-b", 2, "waiting")?,
                    record("worker-a", 3, "stopped")?,
                ],
                None,
            )
        })?;
        let result = observed_seats(&history);
        assert_eq!(result["retained_history_only"], true);
        assert_eq!(result["seats"].as_array().map(Vec::len), Some(2));
        assert_eq!(result["seats"][0]["seat"], "worker-a");
        assert_eq!(result["seats"][0]["retained_events"], 2);
        assert_eq!(result["seats"][0]["last_kind"], "stopped");
        assert_eq!(result["cursor"]["sequence"], 3);
        Ok(())
    }

    #[test]
    fn recent_events_excludes_other_seats_and_preserves_the_feed_cursor() -> Result<()> {
        let history = retained(|_| {
            page(
                vec![
                    record("worker-a", 1, "working")?,
                    record("worker-b", 2, "waiting")?,
                    record("worker-a", 3, "stopped")?,
                ],
                None,
            )
        })?;
        let result = recent_events(&history, &AgentId::new("worker-a")?);
        assert_eq!(result["records"].as_array().map(Vec::len), Some(2));
        assert_eq!(result["records"][0]["sequence"], 1);
        assert_eq!(result["records"][1]["sequence"], 3);
        assert_eq!(result["cursor"]["sequence"], 3);
        Ok(())
    }

    #[test]
    fn pagination_stops_at_the_first_watermark_despite_new_activity() -> Result<()> {
        let mut calls = 0;
        let history = retained(|cursor| {
            calls += 1;
            if calls == 1 {
                assert!(cursor.is_none());
                let mut first = page(vec![record("worker-a", 1, "working")?], None)?;
                first.current_sequence = 2;
                first.has_more = true;
                Ok(first)
            } else {
                assert_eq!(cursor.map(|cursor| cursor.sequence), Some(1));
                page(
                    vec![
                        record("worker-b", 2, "waiting")?,
                        record("worker-c", 3, "stopped")?,
                    ],
                    None,
                )
            }
        })?;
        assert_eq!(calls, 2);
        assert_eq!(history.records.len(), 2);
        assert_eq!(history.cursor.sequence, 2);
        assert_eq!(history.records[1].event.seat.as_str(), "worker-b");
        Ok(())
    }

    #[test]
    fn a_reset_between_pages_fails_instead_of_mixing_daemon_histories() -> Result<()> {
        let mut calls = 0;
        let result = retained(|_| {
            calls += 1;
            let mut next = page(vec![record("worker-a", 1, "working")?], None)?;
            if calls == 1 {
                next.current_sequence = 2;
                next.has_more = true;
            } else {
                next.reset = Some(prismattyc_mux::activity::ActivityReset::Restarted);
            }
            Ok(next)
        });
        assert!(result.is_err());
        assert_eq!(calls, 2);
        Ok(())
    }

    #[test]
    fn a_filtered_empty_tail_page_still_advances_the_cursor() -> Result<()> {
        let page = page(vec![record("worker-b", 7, "waiting")?], None)?;
        let filtered = filtered_page(page, Some(&AgentId::new("worker-a")?));
        assert!(filtered.records.is_empty());
        assert_eq!(filtered.cursor.sequence, 7);
        Ok(())
    }

    #[test]
    fn a_nonadvancing_cursor_stops_the_query_after_two_reads() -> Result<()> {
        let mut calls = 0;
        let result = retained(|_| {
            calls += 1;
            let mut next = page(vec![record("worker-a", 1, "working")?], None)?;
            next.current_sequence = 2;
            next.has_more = true;
            Ok(next)
        });
        assert_eq!(calls, 2);
        assert!(result.is_err());
        Ok(())
    }
}
