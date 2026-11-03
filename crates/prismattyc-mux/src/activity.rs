// SPDX-License-Identifier: MIT
//! Opt-in, process-local activity history and a bounded one-shot client.

use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};

use crate::{
    ControlErrorCode, ControlRequest, ControlResponse, ControlResponseBody, ControlResponseData,
    PROTOCOL_VERSION,
};

const CAPACITY: usize = 512;
const MAX_PAGE: usize = 256;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

pub use prismattyc_core::activity::{
    ActivityAck, ActivityCursor, ActivityInput, ActivityKind, ActivityPage, ActivityRecord,
    ActivityReset, ActivitySource,
};

/// A daemon rejection that retains its machine-readable code without message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityRejected {
    pub code: ControlErrorCode,
}

impl fmt::Display for ActivityRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "activity request rejected ({:?})", self.code)
    }
}

impl std::error::Error for ActivityRejected {}

pub(crate) struct ActivityFeed {
    epoch: String,
    sequence: u64,
    retained: VecDeque<ActivityRecord>,
}

impl ActivityFeed {
    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }
    pub(crate) fn new() -> Result<Self, &'static str> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| "cannot create activity epoch")?;
        Ok(Self {
            epoch: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            sequence: 0,
            retained: VecDeque::with_capacity(CAPACITY),
        })
    }

    pub(crate) fn publish(&mut self, event: ActivityInput) -> Result<ActivityAck, &'static str> {
        event.validate_metadata()?;
        if let Some(prior) = self.retained.iter().find(|r| {
            r.event.seat == event.seat
                && r.event.run_id == event.run_id
                && r.event.event_id == event.event_id
        }) {
            return if prior.event == event {
                Ok(ActivityAck {
                    cursor: self.cursor(prior.sequence),
                    duplicate: true,
                })
            } else {
                Err("activity retry conflicts with retained event")
            };
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or("activity sequence exhausted")?;
        let received_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock precedes activity epoch")?
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        if self.retained.len() == CAPACITY {
            self.retained.pop_front();
        }
        self.retained.push_back(ActivityRecord {
            sequence,
            received_at_ms,
            event,
        });
        self.sequence = sequence;
        Ok(ActivityAck {
            cursor: self.cursor(sequence),
            duplicate: false,
        })
    }

    pub(crate) fn read(
        &self,
        cursor: Option<ActivityCursor>,
        limit: usize,
    ) -> Result<ActivityPage, &'static str> {
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err("activity page limit must be 1..256");
        }
        let oldest = self
            .retained
            .front()
            .map_or(self.sequence.saturating_add(1), |r| r.sequence);
        let reset = cursor.as_ref().and_then(|cursor| {
            if cursor.epoch != self.epoch {
                Some(ActivityReset::Restarted)
            } else if cursor.sequence > self.sequence {
                Some(ActivityReset::AheadOfFeed)
            } else if cursor.sequence < oldest - 1 {
                Some(ActivityReset::RetentionGap)
            } else {
                None
            }
        });
        let after = if reset.is_some() {
            oldest - 1
        } else {
            cursor.map_or(oldest - 1, |c| c.sequence)
        };
        let records: Vec<_> = self
            .retained
            .iter()
            .filter(|r| r.sequence > after)
            .take(limit)
            .cloned()
            .collect();
        let through = records.last().map_or(after, |r| r.sequence);
        Ok(ActivityPage {
            epoch: self.epoch.clone(),
            oldest_sequence: oldest,
            current_sequence: self.sequence,
            records,
            cursor: self.cursor(through),
            has_more: through < self.sequence,
            reset,
            sessions: None,
        })
    }

    fn cursor(&self, sequence: u64) -> ActivityCursor {
        ActivityCursor {
            epoch: self.epoch.clone(),
            sequence,
        }
    }
}

/// Exchange one activity or session-snapshot request over the existing local control socket.
/// Errors omit upstream payloads, paths, and server-supplied message text.
pub fn exchange(socket: &Path, request: ControlRequest) -> Result<ControlResponseData> {
    let (version, request_id, expected) = match &request {
        ControlRequest::PublishActivity {
            version,
            request_id,
            ..
        } => (*version, *request_id, ExpectedResponse::Published),
        ControlRequest::ReadActivity {
            version,
            request_id,
            ..
        } => (*version, *request_id, ExpectedResponse::Activity),
        ControlRequest::Snapshot {
            version,
            request_id,
        } => (*version, *request_id, ExpectedResponse::Snapshot),
        _ => bail!("expected an activity or snapshot request"),
    };
    if version != PROTOCOL_VERSION || request_id == 0 {
        bail!("invalid activity request header");
    }
    let mut bytes =
        serde_json::to_vec(&request).map_err(|_| anyhow!("cannot encode activity request"))?;
    if bytes.len() > 8192 {
        bail!("activity request exceeds 8192 bytes");
    }
    bytes.push(b'\n');
    let deadline = Instant::now() + CLIENT_TIMEOUT;
    let mut stream = crate::local_socket::connect_timeout(socket, CLIENT_TIMEOUT)
        .map_err(|_| anyhow!("cannot connect to activity daemon"))?;
    let mut written = 0;
    while written < bytes.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("activity request deadline exceeded");
        }
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|_| anyhow!("cannot set activity write deadline"))?;
        match stream.write(&bytes[written..]) {
            Ok(0) => bail!("activity request connection closed"),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => bail!("cannot publish activity request"),
        }
    }
    let mut frame = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("activity response deadline exceeded");
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| anyhow!("cannot set activity read deadline"))?;
        let mut chunk = [0u8; 4096];
        let count = stream
            .read(&mut chunk)
            .map_err(|_| anyhow!("cannot read activity response"))?;
        if count == 0 {
            bail!("incomplete activity response frame");
        }
        let through = chunk[..count]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(count, |n| n + 1);
        if frame.len().saturating_add(through) as u64 > MAX_RESPONSE_BYTES {
            bail!("oversized activity response frame");
        }
        frame.extend_from_slice(&chunk[..through]);
        if frame.last() == Some(&b'\n') {
            break;
        }
    }
    let response: ControlResponse =
        serde_json::from_slice(&frame).map_err(|_| anyhow!("invalid activity response"))?;
    if response.version != PROTOCOL_VERSION || response.request_id != request_id {
        bail!("activity response header does not match request");
    }
    match response.body {
        ControlResponseBody::Ok { response }
            if matches!(
                (&response, expected),
                (
                    ControlResponseData::ActivityPublished { .. },
                    ExpectedResponse::Published
                ) | (
                    ControlResponseData::Activity { .. },
                    ExpectedResponse::Activity
                ) | (
                    ControlResponseData::Snapshot { .. },
                    ExpectedResponse::Snapshot
                )
            ) =>
        {
            Ok(response)
        }
        ControlResponseBody::Error { error } => Err(ActivityRejected { code: error.code }.into()),
        _ => bail!("unexpected activity response kind"),
    }
}

#[derive(Clone, Copy)]
enum ExpectedResponse {
    Published,
    Activity,
    Snapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: usize) -> ActivityInput {
        ActivityInput {
            schema_version: 1,
            seat: crate::mailbox::AgentId::new("worker-a").unwrap(),
            run_id: "run-1".into(),
            event_id: format!("event-{id}"),
            source: ActivitySource::Notify,
            kind: ActivityKind::TurnFinished,
            process_ids: Vec::new(),
        }
    }

    #[test]
    fn activity_client_rejects_wrong_requests_headers_and_oversized_payloads() {
        let socket = Path::new("unused-activity-test.sock");
        assert_eq!(
            exchange(
                socket,
                ControlRequest::Ping {
                    version: 1,
                    request_id: 1
                }
            )
            .unwrap_err()
            .to_string(),
            "expected an activity or snapshot request"
        );
        for (version, request_id) in [(2, 1), (1, 0)] {
            assert_eq!(
                exchange(
                    socket,
                    ControlRequest::ReadActivity {
                        version,
                        request_id,
                        cursor: None,
                        limit: 1,
                    }
                )
                .unwrap_err()
                .to_string(),
                "invalid activity request header"
            );
        }
        let mut oversized = event(1);
        oversized.run_id = "x".repeat(8192);
        assert_eq!(
            exchange(
                socket,
                ControlRequest::PublishActivity {
                    version: 1,
                    request_id: 1,
                    event: oversized,
                }
            )
            .unwrap_err()
            .to_string(),
            "activity request exceeds 8192 bytes"
        );
    }

    #[test]
    fn activity_pages_resume_without_skipping_and_empty_poll_keeps_cursor() {
        let mut feed = ActivityFeed::new().unwrap();
        for id in 1..=3 {
            feed.publish(event(id)).unwrap();
        }
        let page = feed.read(None, 2).unwrap();
        assert_eq!(
            page.records.iter().map(|r| r.sequence).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(page.cursor.sequence, 2);
        assert_eq!(page.current_sequence, 3);
        assert!(page.has_more);
        let page = feed.read(Some(page.cursor), 2).unwrap();
        assert_eq!(
            page.records.iter().map(|r| r.sequence).collect::<Vec<_>>(),
            vec![3]
        );
        assert_eq!(page.records[0].event.event_id, "event-3");
        assert!(!page.has_more);
        let page = feed.read(Some(page.cursor), 2).unwrap();
        assert!(page.records.is_empty());
        assert_eq!(page.cursor.sequence, 3);
        assert_eq!(page.reset, None);
    }

    #[test]
    fn activity_retry_is_idempotent_and_conflict_does_not_replace_event() {
        let mut feed = ActivityFeed::new().unwrap();
        let first = feed.publish(event(1)).unwrap();
        assert_eq!(first.cursor.sequence, 1);
        assert!(!first.duplicate);
        let duplicate = feed.publish(event(1)).unwrap();
        assert_eq!(duplicate.cursor, first.cursor);
        assert!(duplicate.duplicate);
        let mut conflict = event(1);
        conflict.kind = ActivityKind::ToolFailed;
        assert_eq!(
            feed.publish(conflict).unwrap_err(),
            "activity retry conflicts with retained event"
        );
        let page = feed.read(None, 256).unwrap();
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].event.kind, ActivityKind::TurnFinished);
        assert_eq!(page.current_sequence, 1);
    }

    #[test]
    fn activity_retention_resets_only_when_cursor_precedes_oldest_minus_one() {
        let mut feed = ActivityFeed::new().unwrap();
        for id in 1..=514 {
            feed.publish(event(id)).unwrap();
        }
        let page = feed.read(Some(feed.cursor(1)), 1).unwrap();
        assert_eq!(page.oldest_sequence, 3);
        assert_eq!(page.reset, Some(ActivityReset::RetentionGap));
        assert_eq!(page.records[0].sequence, 3);
        assert_eq!(page.cursor.sequence, 3);
        let boundary = feed.read(Some(feed.cursor(2)), 256).unwrap();
        assert_eq!(boundary.reset, None);
        assert_eq!(boundary.records.len(), 256);
        assert_eq!(boundary.cursor.sequence, 258);
        assert!(boundary.has_more);
        let retried_evicted = feed.publish(event(1)).unwrap();
        assert_eq!(retried_evicted.cursor.sequence, 515);
        assert!(!retried_evicted.duplicate);
    }

    #[test]
    fn activity_new_epoch_and_ahead_cursor_return_retained_history() {
        let mut feed = ActivityFeed::new().unwrap();
        feed.publish(event(1)).unwrap();
        let other = ActivityFeed::new().unwrap();
        assert_ne!(feed.epoch, other.epoch);
        let restarted = feed.read(Some(other.cursor(100)), 1).unwrap();
        assert_eq!(restarted.reset, Some(ActivityReset::Restarted));
        assert_eq!(restarted.records[0].sequence, 1);
        let ahead = feed.read(Some(feed.cursor(100)), 1).unwrap();
        assert_eq!(ahead.reset, Some(ActivityReset::AheadOfFeed));
        assert_eq!(ahead.cursor.sequence, 1);
    }

    #[test]
    fn activity_invalid_input_leaves_feed_empty() {
        let mut feed = ActivityFeed::new().unwrap();
        let mut invalid = event(1);
        invalid.run_id = "untrusted\ntext".into();
        assert!(feed.publish(invalid).is_err());
        let mut invalid = event(2);
        invalid.schema_version = 2;
        assert_eq!(
            feed.publish(invalid).unwrap_err(),
            "unsupported activity schema"
        );
        let mut invalid = event(3);
        invalid.event_id = "x".repeat(129);
        assert!(feed.publish(invalid).is_err());
        assert!(feed.read(None, 0).is_err());
        assert!(feed.read(None, 257).is_err());
        let page = feed.read(None, 1).unwrap();
        assert!(page.records.is_empty());
        assert_eq!(page.current_sequence, 0);
        assert_eq!(page.cursor.sequence, 0);
    }

    #[test]
    fn activity_same_event_id_is_distinct_between_seats_and_runs() {
        let mut feed = ActivityFeed::new().unwrap();
        feed.publish(event(1)).unwrap();
        let mut other_seat = event(1);
        other_seat.seat = crate::mailbox::AgentId::new("worker-b").unwrap();
        let mut other_run = event(1);
        other_run.run_id = "run-2".into();
        assert_eq!(feed.publish(other_seat).unwrap().cursor.sequence, 2);
        assert_eq!(feed.publish(other_run).unwrap().cursor.sequence, 3);
    }
    #[cfg(unix)]
    fn fake_activity_reply(frame: Vec<u8>) -> String {
        use std::io::{BufRead, BufReader};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::path::PathBuf::from(format!(
            "/tmp/activity-client-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = crate::local_socket::UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let _ = stream.write_all(&frame);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let _ = stream.read_to_end(&mut Vec::new());
        });
        let result = exchange(
            &path,
            ControlRequest::ReadActivity {
                version: PROTOCOL_VERSION,
                request_id: 7,
                cursor: None,
                limit: 1,
            },
        );
        worker.join().unwrap();
        std::fs::remove_file(path).unwrap();
        result.unwrap_err().to_string()
    }

    #[cfg(unix)]
    #[test]
    fn activity_client_rejects_mismatched_headers_kind_and_unbounded_frames() {
        for (version, request_id) in [(2, 7), (1, 6)] {
            let frame = format!("{{\"version\":{version},\"request_id\":{request_id},\"status\":\"ok\",\"response\":{{\"kind\":\"pong\"}}}}\n");
            assert_eq!(
                fake_activity_reply(frame.into_bytes()),
                "activity response header does not match request"
            );
        }
        assert_eq!(fake_activity_reply(b"{\"version\":1,\"request_id\":7,\"status\":\"ok\",\"response\":{\"kind\":\"pong\"}}\n".to_vec()), "unexpected activity response kind");
        assert_eq!(
            fake_activity_reply(b"{\"private-secret-marker\":true}\n".to_vec()),
            "invalid activity response"
        );
        assert_eq!(
            fake_activity_reply(vec![b'x'; MAX_RESPONSE_BYTES as usize + 1]),
            "oversized activity response frame"
        );
        assert_eq!(
            fake_activity_reply(b"{}".to_vec()),
            "incomplete activity response frame"
        );
    }

    #[test]
    fn activity_schema_rejects_extra_raw_payload_fields() {
        let mut input = serde_json::to_value(event(1)).unwrap();
        input["command"] = serde_json::json!("private-secret-marker");
        assert!(serde_json::from_value::<ActivityInput>(input).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn activity_rejection_omits_server_message_text() {
        let frame = b"{\"version\":1,\"request_id\":7,\"status\":\"error\",\"error\":{\"code\":\"activity_disabled\",\"message\":\"private-secret-marker\"}}\n";
        assert_eq!(
            fake_activity_reply(frame.to_vec()),
            "activity request rejected (ActivityDisabled)"
        );
    }
}
