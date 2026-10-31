// SPDX-License-Identifier: MIT
//! Bounded read-only queries over the activity feed.
//!
//! Shared by the `pmux activity` connector and the activity monitor. Every
//! query reads through [`crate::activity::exchange`]; nothing here publishes
//! events, consumes mail, or sends terminal input.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::activity::{
    exchange, ActivityCursor, ActivityKind, ActivityPage, ActivityRecord, ActivityRejected,
    ActivitySource,
};
use crate::control::{ControlErrorCode, ControlRequest, ControlResponseData, PROTOCOL_VERSION};
use crate::mailbox::AgentId;

/// Output schema of the query results below.
pub const QUERY_SCHEMA_VERSION: u16 = 1;
/// Largest page the daemon serves.
pub const MAX_PAGE: usize = 256;
const MAX_RETAINED: usize = 512;
const MAX_QUERY_PAGES: usize = 2;

/// Read one page after `cursor` (`None` reads retained history from the start).
pub fn read_page(
    socket: &Path,
    cursor: Option<ActivityCursor>,
    limit: usize,
) -> Result<ActivityPage> {
    match exchange(
        socket,
        ControlRequest::ReadActivity {
            version: PROTOCOL_VERSION,
            request_id: 1,
            cursor,
            limit,
        },
    )? {
        ControlResponseData::Activity { page } => Ok(page),
        _ => bail!("unexpected activity response"),
    }
}

/// Retained history drained up to the watermark seen on the first page.
pub struct Retained {
    pub epoch: String,
    pub oldest_sequence: u64,
    pub cursor: ActivityCursor,
    pub records: Vec<ActivityRecord>,
}

/// Drain retained history in at most two pages, stopping at the first page's
/// `current_sequence` so a busy producer cannot keep the query running. Fails
/// rather than mixing histories when the daemon resets between pages.
pub fn retained(
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

/// [`retained`] against a live socket.
pub fn read_retained(socket: &Path) -> Result<Retained> {
    retained(|cursor| read_page(socket, cursor, MAX_PAGE))
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
        "schema_version": QUERY_SCHEMA_VERSION,
        "retained_history_only": true,
        "epoch": history.epoch,
        "oldest_sequence": history.oldest_sequence,
        "observed_through_sequence": history.cursor.sequence,
        "cursor": history.cursor
    })
}

/// Seats seen in retained history. Seeing a seat does not mean its process
/// is alive.
pub fn observed_seats(history: &Retained) -> Value {
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

/// Retained events for one seat, oldest first, keeping at most the newest
/// `limit` when one is given.
pub fn recent_events(history: &Retained, seat: &AgentId, limit: Option<usize>) -> Value {
    let records = history
        .records
        .iter()
        .filter(|record| &record.event.seat == seat)
        .collect::<Vec<_>>();
    let skip = limit.map_or(0, |limit| records.len().saturating_sub(limit));
    let mut result = base(history);
    result["seat"] = json!(seat);
    result["truncated"] = json!(skip > 0);
    result["records"] = json!(records[skip..]);
    result
}

/// Drop other seats' records. The cursor still covers them, so the next read
/// does not see them again.
pub fn filtered_page(mut page: ActivityPage, seat: Option<&AgentId>) -> ActivityPage {
    if let Some(seat) = seat {
        page.records.retain(|record| &record.event.seat == seat);
    }
    page
}

/// Parse an `EPOCH:SEQUENCE` cursor, as printed by `poll` and `tail`.
pub fn parse_cursor(text: &str) -> Result<ActivityCursor> {
    let (epoch, sequence) = text
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("cursor must be EPOCH:SEQUENCE"))?;
    if epoch.is_empty() || epoch.len() > 64 || !epoch.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("cursor epoch must be 1 to 64 hex digits");
    }
    let sequence = sequence
        .parse()
        .map_err(|_| anyhow!("cursor sequence must be a non-negative integer"))?;
    Ok(ActivityCursor {
        epoch: epoch.to_owned(),
        sequence,
    })
}

/// `EPOCH:SEQUENCE` form of a cursor.
pub fn format_cursor(cursor: &ActivityCursor) -> String {
    format!("{}:{}", cursor.epoch, cursor.sequence)
}

/// One stateless page: what a caller holding only a cursor string needs to
/// continue. A `reset` means continuity was lost and cached state is stale.
pub fn poll_result(page: ActivityPage, seat: Option<&AgentId>) -> Value {
    let next = format_cursor(&page.cursor);
    json!({
        "schema_version": QUERY_SCHEMA_VERSION,
        "seat_filter": seat,
        "next_cursor": next,
        "page": filtered_page(page, seat)
    })
}

/// When a bounded tail stops on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TailLimits {
    /// Stop after the page that brings the emitted record count to this.
    pub max_events: Option<usize>,
    /// Stop once this much time has passed.
    pub max_duration: Option<Duration>,
}

/// Cursor at the newest event, so a tail started from it sees only new events.
pub fn current_cursor(socket: &Path) -> Result<ActivityCursor> {
    let page = read_page(socket, None, 1)?;
    Ok(ActivityCursor {
        epoch: page.epoch,
        sequence: page.current_sequence,
    })
}

/// Follow the feed from `start` (`None`: retained history first), emitting
/// one JSON value per change: `activity` pages,
/// one `disconnected` notice per outage, and a final `end` with the resume
/// cursor when a limit is reached. Unlimited tails run until `stop` is set or
/// `emit` reports a broken pipe.
pub fn tail(
    mut read: impl FnMut(Option<ActivityCursor>) -> Result<ActivityPage>,
    start: Option<ActivityCursor>,
    seat: Option<&AgentId>,
    limits: TailLimits,
    interval: Duration,
    stop: &AtomicBool,
    mut emit: impl FnMut(&Value) -> io::Result<()>,
) -> Result<()> {
    let started = Instant::now();
    let mut cursor = start;
    let mut disconnected = false;
    let mut emitted = 0usize;
    let mut end = None;
    while !stop.load(Ordering::Relaxed) {
        let message = match read(cursor.clone()) {
            Ok(page) => {
                let changed =
                    cursor.as_ref() != Some(&page.cursor) || page.reset.is_some() || disconnected;
                cursor = Some(page.cursor.clone());
                disconnected = false;
                changed.then(|| {
                    let page = filtered_page(page, seat);
                    emitted += page.records.len();
                    json!({"type": "activity", "seat_filter": seat, "page": page})
                })
            }
            Err(_) if !disconnected => {
                disconnected = true;
                Some(json!({"type": "disconnected", "retrying": true, "cursor": cursor}))
            }
            Err(_) => None,
        };
        if let Some(message) = message {
            match emit(&message) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
                Err(error) => {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    return Err(error.into());
                }
            }
        }
        if limits.max_events.is_some_and(|max| emitted >= max) {
            end = Some("max_events");
            break;
        }
        if limits
            .max_duration
            .is_some_and(|max| started.elapsed() >= max)
        {
            end = Some("max_seconds");
            break;
        }
        thread::sleep(interval);
    }
    if let Some(reason) = end {
        let resume = cursor.as_ref().map(format_cursor);
        let message = json!({"type": "end", "reason": reason, "next_cursor": resume});
        match emit(&message) {
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => return Err(error.into()),
            _ => {}
        }
    }
    Ok(())
}

/// Stable error code for a failed query, for callers that parse JSON.
pub fn error_code(error: &anyhow::Error) -> &'static str {
    if let Some(rejected) = error.downcast_ref::<ActivityRejected>() {
        return match rejected.code {
            ControlErrorCode::ActivityDisabled => "feed_disabled",
            _ => "request_rejected",
        };
    }
    let text = error.to_string();
    if text.starts_with("cannot connect to activity daemon") {
        "daemon_unreachable"
    } else if text.contains("changed during the bounded query")
        || text.contains("cursor did not advance")
        || text.contains("ended before the query watermark")
    {
        "feed_changed_retry"
    } else {
        "request_failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::ActivityReset;
    use std::cell::Cell;

    fn record(seat: &str, sequence: u64, kind: &str) -> Result<ActivityRecord> {
        Ok(serde_json::from_value(json!({
            "sequence": sequence,
            "received_at_ms": 1000,
            "event": {
                "schema_version": 1,
                "seat": seat,
                "run_id": "run-1",
                "event_id": format!("event-{sequence}"),
                "source": "runtime",
                "kind": kind
            }
        }))?)
    }

    fn page(records: Vec<ActivityRecord>, reset: Option<&str>) -> Result<ActivityPage> {
        let sequence = records.last().map_or(0, |record| record.sequence);
        Ok(serde_json::from_value(json!({
            "epoch": "e0",
            "oldest_sequence": 1,
            "current_sequence": sequence,
            "records": records,
            "cursor": {"epoch": "e0", "sequence": sequence},
            "has_more": false,
            "reset": reset
        }))?)
    }

    fn three() -> Result<Retained> {
        retained(|_| {
            page(
                vec![
                    record("worker-a", 1, "working")?,
                    record("worker-b", 2, "waiting")?,
                    record("worker-a", 3, "stopped")?,
                ],
                None,
            )
        })
    }

    #[test]
    fn seat_summary_counts_events_and_uses_the_latest_kind() -> Result<()> {
        let result = observed_seats(&three()?);
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
        let result = recent_events(&three()?, &AgentId::new("worker-a")?, None);
        assert_eq!(result["records"].as_array().map(Vec::len), Some(2));
        assert_eq!(result["records"][0]["sequence"], 1);
        assert_eq!(result["records"][1]["sequence"], 3);
        assert_eq!(result["truncated"], false);
        assert_eq!(result["cursor"]["sequence"], 3);
        Ok(())
    }

    #[test]
    fn recent_limit_keeps_the_newest_events_and_says_so() -> Result<()> {
        let result = recent_events(&three()?, &AgentId::new("worker-a")?, Some(1));
        assert_eq!(result["records"].as_array().map(Vec::len), Some(1));
        assert_eq!(result["records"][0]["sequence"], 3);
        assert_eq!(result["truncated"], true);
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
                next.reset = Some(ActivityReset::Restarted);
            }
            Ok(next)
        });
        let error = result.err().expect("reset must fail the query");
        assert_eq!(error_code(&error), "feed_changed_retry");
        assert_eq!(calls, 2);
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

    #[test]
    fn a_filtered_empty_page_still_advances_the_cursor() -> Result<()> {
        let page = page(vec![record("worker-b", 7, "waiting")?], None)?;
        let result = poll_result(page, Some(&AgentId::new("worker-a")?));
        assert_eq!(result["page"]["records"].as_array().map(Vec::len), Some(0));
        assert_eq!(result["next_cursor"], "e0:7");
        Ok(())
    }

    #[test]
    fn cursors_round_trip_and_reject_malformed_text() -> Result<()> {
        let cursor = parse_cursor("0a1b:42")?;
        assert_eq!(cursor.epoch, "0a1b");
        assert_eq!(cursor.sequence, 42);
        assert_eq!(format_cursor(&cursor), "0a1b:42");
        for bad in ["42", ":1", "e0:", "e0:-1", "zz:1", "e0:1.5"] {
            assert!(parse_cursor(bad).is_err(), "{bad} should be rejected");
        }
        Ok(())
    }

    #[test]
    fn tail_stops_after_max_events_and_reports_the_resume_cursor() -> Result<()> {
        let sequence = Cell::new(0u64);
        let stop = AtomicBool::new(false);
        let mut lines = Vec::new();
        tail(
            |_| {
                sequence.set(sequence.get() + 1);
                page(vec![record("worker-a", sequence.get(), "working")?], None)
            },
            None,
            None,
            TailLimits {
                max_events: Some(3),
                max_duration: None,
            },
            Duration::ZERO,
            &stop,
            |value| {
                lines.push(value.clone());
                Ok(())
            },
        )?;
        assert_eq!(lines.len(), 4);
        assert!(lines[..3].iter().all(|line| line["type"] == "activity"));
        assert_eq!(lines[3]["type"], "end");
        assert_eq!(lines[3]["reason"], "max_events");
        assert_eq!(lines[3]["next_cursor"], "e0:3");
        Ok(())
    }

    #[test]
    fn tail_counts_only_the_filtered_seat_toward_max_events() -> Result<()> {
        let sequence = Cell::new(0u64);
        let stop = AtomicBool::new(false);
        let mut ends = Vec::new();
        tail(
            |_| {
                sequence.set(sequence.get() + 1);
                let seat = if sequence.get().is_multiple_of(2) {
                    "worker-a"
                } else {
                    "worker-b"
                };
                page(vec![record(seat, sequence.get(), "working")?], None)
            },
            None,
            Some(&AgentId::new("worker-a")?),
            TailLimits {
                max_events: Some(2),
                max_duration: None,
            },
            Duration::ZERO,
            &stop,
            |value| {
                if value["type"] == "end" {
                    ends.push(value.clone());
                }
                Ok(())
            },
        )?;
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["next_cursor"], "e0:4");
        Ok(())
    }

    #[test]
    fn tail_reports_one_disconnect_per_outage_and_stops_on_time() -> Result<()> {
        let stop = AtomicBool::new(false);
        let mut lines = Vec::new();
        tail(
            |_| Err(anyhow!("cannot connect to activity daemon")),
            None,
            None,
            TailLimits {
                max_events: None,
                max_duration: Some(Duration::from_millis(30)),
            },
            Duration::from_millis(5),
            &stop,
            |value| {
                lines.push(value.clone());
                Ok(())
            },
        )?;
        let kinds: Vec<_> = lines.iter().map(|line| line["type"].clone()).collect();
        assert_eq!(kinds, vec![json!("disconnected"), json!("end")]);
        assert_eq!(lines[1]["reason"], "max_seconds");
        assert_eq!(lines[1]["next_cursor"], Value::Null);
        Ok(())
    }

    #[test]
    fn tail_from_a_start_cursor_reads_after_it_and_skips_unchanged_pages() -> Result<()> {
        let stop = AtomicBool::new(false);
        let mut seen = Vec::new();
        let mut lines = Vec::new();
        tail(
            |cursor| {
                seen.push(cursor.as_ref().map(|cursor| cursor.sequence));
                if seen.len() < 3 {
                    page(vec![], None).map(|mut quiet| {
                        quiet.cursor.sequence = 5;
                        quiet
                    })
                } else {
                    page(vec![record("worker-a", 6, "working")?], None)
                }
            },
            Some(ActivityCursor {
                epoch: "e0".into(),
                sequence: 5,
            }),
            None,
            TailLimits {
                max_events: Some(1),
                max_duration: None,
            },
            Duration::ZERO,
            &stop,
            |value| {
                lines.push(value.clone());
                Ok(())
            },
        )?;
        assert_eq!(seen, vec![Some(5), Some(5), Some(5)]);
        assert_eq!(
            lines.len(),
            2,
            "quiet pages at the start cursor are not emitted"
        );
        assert_eq!(lines[0]["page"]["records"][0]["sequence"], 6);
        assert_eq!(lines[1]["next_cursor"], "e0:6");
        Ok(())
    }

    #[test]
    fn connection_failures_map_to_daemon_unreachable() {
        assert_eq!(
            error_code(&anyhow!("cannot connect to activity daemon")),
            "daemon_unreachable"
        );
        assert_eq!(
            error_code(&anyhow!("invalid activity response")),
            "request_failed"
        );
    }

    #[test]
    fn a_disabled_feed_maps_to_feed_disabled() {
        let disabled = anyhow::Error::new(ActivityRejected {
            code: ControlErrorCode::ActivityDisabled,
        });
        assert_eq!(error_code(&disabled), "feed_disabled");
        let other = anyhow::Error::new(ActivityRejected {
            code: ControlErrorCode::InvalidRequest,
        });
        assert_eq!(error_code(&other), "request_rejected");
    }
}
