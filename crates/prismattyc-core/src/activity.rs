// SPDX-License-Identifier: MIT
//! Metadata-only activity values shared by native hosts and portable consumers.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityInput {
    pub schema_version: u16,
    pub seat: crate::AgentId,
    pub run_id: String,
    pub event_id: String,
    pub source: ActivitySource,
    pub kind: ActivityKind,
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
    }
}
