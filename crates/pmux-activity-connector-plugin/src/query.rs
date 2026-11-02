// SPDX-License-Identifier: MIT
//! Bounded pure queries over caller-supplied metadata pages.

use anyhow::{anyhow, bail};
use prismattyc_core::{
    activity::{ActivityCursor, ActivityKind, ActivityPage, ActivityRecord, ActivitySource},
    AgentId,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const QUERY_SCHEMA_VERSION: u16 = 1;
const MAX_RETAINED: usize = 512;
const MAX_QUERY_PAGES: usize = 2;

/// Retained history drained up to the watermark seen on the first page.
pub struct Retained {
    pub epoch: String,
    pub oldest_sequence: u64,
    pub cursor: ActivityCursor,
    pub records: Vec<ActivityRecord>,
    pub sessions: Option<prismattyc_core::activity::ActivitySessions>,
}

/// Return the first bounded page at or after a Unix-millisecond timestamp.
/// The cursor advances through the current watermark when no retained event
/// matches, so subsequent polls do not replay old records.
pub fn since_time(history: &Retained, since_ms: u64, limit: usize) -> ActivityPage {
    let matching = history
        .records
        .iter()
        .filter(|record| record.received_at_ms >= since_ms)
        .collect::<Vec<_>>();
    let records = matching
        .iter()
        .take(limit)
        .map(|record| (*record).clone())
        .collect::<Vec<_>>();
    let sequence = records
        .last()
        .map_or(history.cursor.sequence, |record| record.sequence);
    let retention_gap = history
        .records
        .first()
        .is_some_and(|record| since_ms < record.received_at_ms);
    ActivityPage {
        epoch: history.epoch.clone(),
        oldest_sequence: history.oldest_sequence,
        current_sequence: history.cursor.sequence,
        records,
        cursor: ActivityCursor {
            epoch: history.epoch.clone(),
            sequence,
        },
        has_more: sequence < history.cursor.sequence,
        reset: retention_gap.then_some(prismattyc_core::activity::ActivityReset::RetentionGap),
    }
}

/// Drain retained history in at most two pages, stopping at the first page's
/// `current_sequence` so a busy producer cannot keep the query running. Fails
/// rather than mixing histories when the daemon resets between pages.
pub fn retained(
    mut read: impl FnMut(Option<ActivityCursor>) -> anyhow::Result<ActivityPage>,
) -> anyhow::Result<Retained> {
    let mut page = read(None)?;
    let epoch = page.epoch.clone();
    let oldest_sequence = page.oldest_sequence;
    let watermark = page.current_sequence;
    let mut records = Vec::new();
    let mut prior_sequence = None;
    let mut sessions = page.sessions.clone();
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
                sessions,
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
        sessions = page.sessions.clone();
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
    let observations = history
        .sessions
        .as_ref()
        .map(|snapshot| {
            snapshot
                .seats
                .iter()
                .map(|session| session.observation(snapshot.observed_at_ms))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut seats = BTreeMap::new();
    for record in &observations {
        seats.insert(
            &record.event.seat,
            ObservedSeat {
                seat: &record.event.seat,
                first_retained_sequence: 0,
                last_sequence: 0,
                last_received_at_ms: record.received_at_ms,
                last_source: record.event.source,
                last_kind: record.event.kind,
                retained_events: 0,
            },
        );
    }
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
        if entry.retained_events == 0 {
            entry.first_retained_sequence = record.sequence;
        }
        entry.last_sequence = record.sequence;
        entry.last_received_at_ms = record.received_at_ms;
        entry.last_source = record.event.source;
        entry.last_kind = record.event.kind;
        entry.retained_events += 1;
    }
    let mut result = base(history);
    result["retained_history_only"] = json!(history.sessions.is_none());
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
        if let Some(sessions) = &mut page.sessions {
            sessions.seats.retain(|session| &session.seat == seat);
        }
    }
    page
}

/// Parse an `EPOCH:SEQUENCE` cursor, as printed by `poll` and `tail`.
pub fn parse_cursor(text: &str) -> anyhow::Result<ActivityCursor> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use prismattyc_core::activity::ActivityReset;
    #[test]
    fn seats_and_filtered_poll_include_sessions_without_retained_events() -> Result<()> {
        let mut first = page(vec![], None)?;
        first.sessions = Some(serde_json::from_value(json!({
            "observed_at_ms": 5000, "seats": [
                {"seat":"quiet","session_id":1,"state":"waiting","last_output_at_ms":null},
                {"seat":"busy","session_id":2,"state":"working","last_output_at_ms":4900}
            ]
        }))?);
        let history = retained(|_| Ok(first.clone()))?;
        let result = observed_seats(&history);
        assert_eq!(result["retained_history_only"], false);
        assert_eq!(result["seats"][0]["seat"], "busy");
        assert_eq!(result["seats"][0]["last_kind"], "working");
        assert_eq!(result["seats"][1]["seat"], "quiet");
        assert_eq!(result["seats"][1]["retained_events"], 0);
        assert_eq!(history.cursor.sequence, 0);
        let filtered = filtered_page(first, Some(&AgentId::new("quiet")?));
        assert_eq!(filtered.sessions.unwrap().seats.len(), 1);
        Ok(())
    }

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
    fn time_poll_filters_retained_events_and_returns_a_resumable_cursor() -> Result<()> {
        let mut history = three()?;
        history.records[1].received_at_ms = 2_000;
        history.records[2].received_at_ms = 3_000;
        let page = since_time(&history, 2_000, 1);
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].sequence, 2);
        assert_eq!(page.cursor.sequence, 2);
        assert!(page.has_more);
        assert_eq!(page.reset, None);
        Ok(())
    }

    #[test]
    fn time_poll_marks_a_gap_when_the_requested_time_predates_retained_history() -> Result<()> {
        let history = three()?;
        let page = since_time(&history, 0, 256);
        assert_eq!(page.records.len(), 3);
        assert_eq!(page.reset, Some(ActivityReset::RetentionGap));
        assert_eq!(page.cursor.sequence, 3);
        Ok(())
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
        assert_eq!(
            error.to_string(),
            "activity feed changed during the bounded query; retry"
        );
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
}
