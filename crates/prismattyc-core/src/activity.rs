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
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
        let field_count = 8 - usize::from(omit_empty_process_ids) - usize::from(omit_missing_detail);
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
        }
        Ok(())
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
        };
        assert_eq!(event.validate_metadata(), Ok(()));
        event.detail.as_mut().unwrap().command = Some("line\nbreak".into());
        assert!(event.validate_metadata().is_err());
        event.detail.as_mut().unwrap().command = Some("x".repeat(257));
        assert!(event.validate_metadata().is_err());
    }
}
