// SPDX-License-Identifier: MIT
//! Metadata-only activity values shared by native hosts and portable consumers.

use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityInput {
    pub schema_version: u16,
    pub seat: crate::AgentId,
    pub run_id: String,
    pub event_id: String,
    pub source: ActivitySource,
    pub kind: ActivityKind,
    #[serde(default)]
    pub process_ids: Vec<u32>,
    /// Safe, bounded fields for human-readable event details. Producers must
    /// sanitize hook payloads before publication; omitted fields stay unknown.
    #[serde(default)]
    pub detail: Option<ActivityDetail>,
}

impl Serialize for ActivityInput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Postcard encodes structs positionally, so omitted fields would shift
        // following fields in an enclosing activity page.
        let human_readable = serializer.is_human_readable();
        let omit_empty_process_ids = human_readable && self.process_ids.is_empty();
        let omit_missing_detail = human_readable && self.detail.is_none();
        let field_count =
            8 - usize::from(omit_empty_process_ids) - usize::from(omit_missing_detail);
        let mut state = serializer.serialize_struct("ActivityInput", field_count)?;
        state.serialize_field("schema_version", &self.schema_version)?;
        state.serialize_field("seat", &self.seat)?;
        state.serialize_field("run_id", &self.run_id)?;
        state.serialize_field("event_id", &self.event_id)?;
        state.serialize_field("source", &self.source)?;
        state.serialize_field("kind", &self.kind)?;
        if !omit_empty_process_ids {
            state.serialize_field("process_ids", &self.process_ids)?;
        }
        if !omit_missing_detail {
            state.serialize_field("detail", &self.detail)?;
        }
        state.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ActivityDetail {
    pub tool: Option<String>,
    pub file: Option<String>,
    pub command: Option<String>,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub task: Option<String>,
    /// Opaque correlation key; the UI hides it unless diagnostic IDs are on.
    pub operation_id: Option<String>,
    pub model: Option<String>,
    /// Letter or room message id. Same character rules as `event_id`.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Room id for a room arrival. Same character rules as `event_id`.
    #[serde(default)]
    pub room_id: Option<String>,
    /// Sender mailbox id. Never a display name or address label.
    #[serde(default)]
    pub sender_id: Option<String>,
    /// Open letters for the recipient after this letter was stored.
    #[serde(default)]
    pub count: Option<u32>,
}

impl ActivityInput {
    pub fn validate_metadata(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("unsupported activity schema");
        }
        if !valid_identifier(&self.run_id) || !valid_identifier(&self.event_id) {
            return Err("activity identifiers must be 1..128 ASCII identifier characters");
        }
        if self.process_ids.len() > 256 || self.process_ids.contains(&0) {
            return Err("invalid activity process metadata");
        }
        if let Some(detail) = &self.detail {
            for value in [
                detail.tool.as_deref(),
                detail.file.as_deref(),
                detail.command.as_deref(),
                detail.task.as_deref(),
                detail.model.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if value.len() > 256 || value.chars().any(char::is_control) {
                    return Err("activity detail fields must be at most 256 printable bytes");
                }
            }
            if detail
                .operation_id
                .as_deref()
                .is_some_and(|value| !valid_identifier(value))
            {
                return Err("activity operation identifier is invalid");
            }
            for value in [
                detail.message_id.as_deref(),
                detail.room_id.as_deref(),
                detail.sender_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !valid_identifier(value) {
                    return Err(
                        "activity arrival identifiers must be 1..128 ASCII identifier characters",
                    );
                }
            }
        }
        Ok(())
    }

    /// Metadata for a stored letter or a room push hint.
    /// `message_id` is also the event id. A room id becomes `run_id`.
    /// The sender id is copied into `operation_id` so a portable reader that
    /// only has the original detail fields still sees an id, not display text.
    pub fn arrival(
        seat: crate::AgentId,
        kind: ActivityKind,
        message_id: &str,
        room_id: Option<&str>,
        sender_id: Option<&str>,
        count: Option<u32>,
    ) -> Result<Self, &'static str> {
        let run_id = match kind {
            ActivityKind::MailReceived | ActivityKind::MailSent => "mail".to_string(),
            ActivityKind::RoomMessage => room_id
                .filter(|id| valid_identifier(id))
                .ok_or("room arrival requires a room id")?
                .to_string(),
            _ => return Err("arrival events are mail or room only"),
        };
        let event = Self {
            schema_version: 1,
            seat,
            run_id,
            event_id: message_id.to_string(),
            source: ActivitySource::Notify,
            kind,
            process_ids: Vec::new(),
            detail: Some(ActivityDetail {
                message_id: Some(message_id.to_string()),
                room_id: room_id.map(str::to_string),
                sender_id: sender_id.map(str::to_string),
                count,
                operation_id: sender_id.map(str::to_string),
                ..ActivityDetail::default()
            }),
        };
        event.validate_metadata()?;
        Ok(event)
    }
}

fn valid_identifier(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_:.".contains(&byte))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    Notify,
    Hooks,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    SessionStarted,
    SessionFinished,
    TurnStarted,
    TurnFinished,
    ToolStarted,
    ToolFinished,
    ToolFailed,
    CommandStarted,
    CommandFinished,
    CommandFailed,
    EditStarted,
    EditFinished,
    EditFailed,
    TestStarted,
    TestPassed,
    TestFailed,
    Working,
    Waiting,
    Stopped,
    MailSent,
    MailReceived,
    RoomMessage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRecord {
    pub sequence: u64,
    pub received_at_ms: u64,
    pub event: ActivityInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityCursor {
    pub epoch: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityReset {
    Restarted,
    RetentionGap,
    AheadOfFeed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityPage {
    pub epoch: String,
    pub oldest_sequence: u64,
    pub current_sequence: u64,
    pub records: Vec<ActivityRecord>,
    pub cursor: ActivityCursor,
    pub has_more: bool,
    pub reset: Option<ActivityReset>,
    /// Current daemon sessions, independent of retained history. Older sources omit it.
    // Worker messages use postcard: an absent value still needs its positional None tag.
    #[serde(default)]
    pub sessions: Option<ActivitySessions>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySessions {
    pub observed_at_ms: u64,
    pub seats: Vec<ActivitySession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySession {
    pub seat: crate::AgentId,
    pub session_id: u64,
    pub state: ActivitySessionState,
    pub last_output_at_ms: Option<u64>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub quota: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySessionState {
    Working,
    Waiting,
    Idle,
    Stuck,
    Stopped,
}

impl ActivitySession {
    /// A display fallback, never inserted into event history or used as a cursor.
    pub fn observation(&self, observed_at_ms: u64) -> ActivityRecord {
        ActivityRecord {
            sequence: 0,
            received_at_ms: observed_at_ms,
            event: ActivityInput {
                schema_version: 1,
                seat: self.seat.clone(),
                run_id: format!("session-{}", self.session_id),
                event_id: "session-snapshot".into(),
                source: ActivitySource::Runtime,
                kind: match self.state {
                    ActivitySessionState::Working => ActivityKind::Working,
                    ActivitySessionState::Waiting => ActivityKind::Waiting,
                    ActivitySessionState::Idle => ActivityKind::Waiting,
                    ActivitySessionState::Stuck => ActivityKind::Working,
                    ActivitySessionState::Stopped => ActivityKind::Stopped,
                },
                detail: None,
                process_ids: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityAck {
    pub cursor: ActivityCursor,
    pub duplicate: bool,
}

/// Upper bound for one activity wait, including a clock that never advances.
pub const MAX_ACTIVITY_WAIT_MS: u64 = 60_000;
/// Plug-in calls return before the host epoch budget. A longer watch repeats the wait.
pub const PLUGIN_ACTIVITY_WAIT_MS: u64 = 750;
pub const WAIT_SLICE_MS: u64 = 50;

/// One retained feed is at most this many pages when each page holds one record.
const MAX_BACKLOG_PAGES: u32 = 512;

/// Poll until `seat` has an event of `kinds`, or the timeout elapses.
/// A match already retained returns without sleeping, including one past the
/// first page. Each sleep is at most [`WAIT_SLICE_MS`]. A stuck clock still
/// stops: the wait count cannot exceed `timeout / WAIT_SLICE_MS + 1`.
pub fn wait_for_activity<E>(
    mut poll: impl FnMut(Option<&ActivityCursor>) -> Result<ActivityPage, E>,
    mut sleep: impl FnMut(u64),
    mut now_ms: impl FnMut() -> u64,
    seat: &crate::AgentId,
    kinds: &[ActivityKind],
    timeout_ms: u64,
) -> Result<ActivityPage, E> {
    let timeout_ms = timeout_ms.min(MAX_ACTIVITY_WAIT_MS);
    let deadline = now_ms().saturating_add(timeout_ms);
    let max_spins = u32::try_from(timeout_ms / WAIT_SLICE_MS)
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let mut spins = 0u32;
    loop {
        let mut after = None;
        let mut pages = 0u32;
        let mut page = loop {
            let mut page = poll(after.as_ref())?;
            pages += 1;
            let matched = page
                .records
                .iter()
                .filter(|record| &record.event.seat == seat && kinds.contains(&record.event.kind))
                .cloned()
                .collect::<Vec<_>>();
            if !matched.is_empty() {
                page.records = matched;
                return Ok(page);
            }
            let stalled = after.as_ref().is_some_and(|cursor| {
                cursor.epoch == page.cursor.epoch && cursor.sequence >= page.cursor.sequence
            });
            if !page.has_more || stalled || pages >= MAX_BACKLOG_PAGES {
                break page;
            }
            after = Some(page.cursor.clone());
        };
        if timeout_ms == 0 || now_ms() >= deadline || spins >= max_spins {
            page.records.clear();
            return Ok(page);
        }
        let remaining = deadline.saturating_sub(now_ms());
        let slice = remaining.clamp(1, WAIT_SLICE_MS);
        sleep(slice);
        spins += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_keep_the_existing_metadata_json_contract() {
        let record = ActivityRecord {
            sequence: 7,
            received_at_ms: 1234,
            event: ActivityInput {
                schema_version: 1,
                seat: crate::AgentId::new("fixture-seat").unwrap(),
                run_id: "run-1".into(),
                event_id: "event-1".into(),
                source: ActivitySource::Hooks,
                kind: ActivityKind::TurnStarted,
                detail: None,
                process_ids: Vec::new(),
            },
        };
        assert_eq!(
            serde_json::to_value(record).unwrap(),
            serde_json::json!({
                "sequence": 7,
                "received_at_ms": 1234,
                "event": {
                    "schema_version": 1,
                    "seat": "fixture-seat",
                    "run_id": "run-1",
                    "event_id": "event-1",
                    "source": "hooks",
                    "kind": "turn_started"
                }
            })
        );
    }

    #[test]
    fn input_rejects_content_fields_and_invalid_identity() {
        let with_content = r#"{"schema_version":1,"seat":"fixture-seat","run_id":"r","event_id":"e","source":"hooks","kind":"waiting","prompt":"content"}"#;
        assert!(serde_json::from_str::<ActivityInput>(with_content).is_err());
        let invalid_seat = r#"{"schema_version":1,"seat":"INVALID","run_id":"r","event_id":"e","source":"hooks","kind":"waiting"}"#;
        assert!(serde_json::from_str::<ActivityInput>(invalid_seat).is_err());
        let invalid_process = r#"{"schema_version":1,"seat":"fixture-seat","run_id":"r","event_id":"e","source":"hooks","kind":"waiting","process_ids":[0]}"#;
        assert!(serde_json::from_str::<ActivityInput>(invalid_process)
            .unwrap()
            .validate_metadata()
            .is_err());
    }

    #[test]
    fn details_are_bounded_and_reject_control_characters() {
        let mut event = ActivityInput {
            schema_version: 1,
            seat: crate::AgentId::new("fixture-seat").unwrap(),
            run_id: "r".into(),
            event_id: "e".into(),
            source: ActivitySource::Hooks,
            kind: ActivityKind::CommandStarted,
            detail: Some(ActivityDetail {
                command: Some("cargo check".into()),
                ..ActivityDetail::default()
            }),
            process_ids: Vec::new(),
        };
        assert_eq!(event.validate_metadata(), Ok(()));
        event.detail.as_mut().unwrap().command = Some("line\nbreak".into());
        assert!(event.validate_metadata().is_err());
        event.detail.as_mut().unwrap().command = Some("x".repeat(257));
        assert!(event.validate_metadata().is_err());
    }

    #[test]
    fn arrival_metadata_keeps_ids_and_rejects_content_and_display_text() {
        let seat = crate::AgentId::new("worker-a").unwrap();
        let mail = ActivityInput::arrival(
            seat.clone(),
            ActivityKind::MailReceived,
            "msg:00000000000000000042",
            None,
            Some("operator-b"),
            Some(2),
        )
        .unwrap();
        assert_eq!(mail.kind, ActivityKind::MailReceived);
        assert_eq!(mail.run_id, "mail");
        assert_eq!(mail.event_id, "msg:00000000000000000042");
        assert_eq!(mail.source, ActivitySource::Notify);
        let detail = mail.detail.as_ref().unwrap();
        assert_eq!(detail.sender_id.as_deref(), Some("operator-b"));
        assert_eq!(detail.operation_id.as_deref(), Some("operator-b"));
        assert_eq!(detail.count, Some(2));
        assert!(detail.tool.is_none() && detail.command.is_none() && detail.file.is_none());
        let encoded = serde_json::to_string(&mail).unwrap();
        assert!(!encoded.contains("subject"));
        assert!(!encoded.contains("hello"));

        let with_body = r#"{"schema_version":1,"seat":"worker-a","run_id":"mail","event_id":"msg:00000000000000000042","source":"notify","kind":"mail_received","detail":{"message_id":"msg:00000000000000000042","body":"hello"}}"#;
        assert!(serde_json::from_str::<ActivityInput>(with_body).is_err());
        let with_subject = r#"{"schema_version":1,"seat":"worker-a","run_id":"mail","event_id":"msg:00000000000000000042","source":"notify","kind":"mail_received","detail":{"subject":"hello"}}"#;
        assert!(serde_json::from_str::<ActivityInput>(with_subject).is_err());

        assert!(ActivityInput::arrival(
            seat.clone(),
            ActivityKind::MailReceived,
            "msg:00000000000000000042",
            None,
            Some("not a name"),
            None,
        )
        .is_err());
        assert!(ActivityInput::arrival(
            seat.clone(),
            ActivityKind::RoomMessage,
            "m-1",
            None,
            Some("operator-b"),
            None,
        )
        .is_err());
        let room = ActivityInput::arrival(
            seat,
            ActivityKind::RoomMessage,
            "m-1",
            Some("room-7"),
            Some("operator-b"),
            None,
        )
        .unwrap();
        assert_eq!(room.run_id, "room-7");
        assert_eq!(room.kind, ActivityKind::RoomMessage);
        assert_eq!(room.detail.unwrap().room_id.as_deref(), Some("room-7"));
    }

    #[test]
    fn wait_returns_a_queued_match_without_sleeping_and_stops_when_the_clock_does_not_move() {
        let seat = crate::AgentId::new("worker-a").unwrap();
        let queued = ActivityInput::arrival(
            seat.clone(),
            ActivityKind::MailReceived,
            "msg:00000000000000000007",
            None,
            Some("operator-b"),
            Some(1),
        )
        .unwrap();
        let page = ActivityPage {
            epoch: "e0".into(),
            oldest_sequence: 1,
            current_sequence: 1,
            records: vec![ActivityRecord {
                sequence: 1,
                received_at_ms: 10,
                event: queued,
            }],
            cursor: ActivityCursor {
                epoch: "e0".into(),
                sequence: 1,
            },
            has_more: false,
            reset: None,
            sessions: None,
        };
        let mut polls = 0;
        let mut slept = 0u64;
        let found = wait_for_activity(
            |_| {
                polls += 1;
                Ok::<_, &'static str>(page.clone())
            },
            |slice| slept += slice,
            || 0,
            &seat,
            &[ActivityKind::MailReceived],
            60_000,
        )
        .unwrap();
        assert_eq!(polls, 1);
        assert_eq!(slept, 0);
        assert_eq!(found.records.len(), 1);
        assert_eq!(found.records[0].event.event_id, "msg:00000000000000000007");

        let mut polls = 0;
        let mut slices = Vec::new();
        let mut empty = page.clone();
        empty.records.clear();
        empty.current_sequence = 0;
        let timed_out = wait_for_activity(
            |_| {
                polls += 1;
                Ok::<_, &'static str>(empty.clone())
            },
            |slice| slices.push(slice),
            || 0,
            &seat,
            &[ActivityKind::RoomMessage],
            1_000,
        )
        .unwrap();
        assert!(timed_out.records.is_empty());
        assert!(polls <= 22, "{polls}");
        assert!(slices.iter().all(|slice| *slice <= WAIT_SLICE_MS));
        assert!(slices.len() < polls);
    }

    #[test]
    fn wait_returns_a_match_after_the_first_retained_page() {
        let seat = crate::AgentId::new("worker-a").unwrap();
        let mut polls = 0u32;
        let mut slept = 0u64;
        let found = wait_for_activity(
            |after| {
                polls += 1;
                let start = after.map(|cursor| cursor.sequence).unwrap_or(0);
                let records = (start + 1..=257)
                    .take(256)
                    .map(|sequence| {
                        let event_id = format!("event-{sequence}");
                        let event = if sequence == 257 {
                            ActivityInput::arrival(
                                seat.clone(),
                                ActivityKind::MailReceived,
                                &event_id,
                                None,
                                Some("operator-b"),
                                None,
                            )
                            .unwrap()
                        } else {
                            ActivityInput {
                                schema_version: 1,
                                seat: seat.clone(),
                                run_id: "run-1".into(),
                                event_id,
                                source: ActivitySource::Runtime,
                                kind: ActivityKind::Working,
                                detail: None,
                                process_ids: Vec::new(),
                            }
                        };
                        ActivityRecord {
                            sequence,
                            received_at_ms: sequence,
                            event,
                        }
                    })
                    .collect::<Vec<_>>();
                let through = records
                    .last()
                    .map(|record| record.sequence)
                    .unwrap_or(start);
                Ok::<_, &'static str>(ActivityPage {
                    epoch: "e0".into(),
                    oldest_sequence: 1,
                    current_sequence: 257,
                    records,
                    cursor: ActivityCursor {
                        epoch: "e0".into(),
                        sequence: through,
                    },
                    has_more: through < 257,
                    reset: None,
                    sessions: None,
                })
            },
            |slice| slept += slice,
            || 0,
            &seat,
            &[ActivityKind::MailReceived],
            0,
        )
        .unwrap();
        assert_eq!(polls, 2);
        assert_eq!(slept, 0);
        assert_eq!(found.records.len(), 1);
        assert_eq!(found.records[0].sequence, 257);
        assert_eq!(found.records[0].event.event_id, "event-257");
    }
}
