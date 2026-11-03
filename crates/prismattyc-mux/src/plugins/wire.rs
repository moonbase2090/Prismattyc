// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::io::{Read, Write};

use anyhow::{ensure, Context, Result};
use pmux_plugin_api::host::pmux::{activity::read, plugin::lifecycle};
use prismattyc_core::activity::{ActivityCursor, ActivityPage};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

const MAX_MESSAGE_BYTES: usize = 24 * 1024 * 1024;

pub(crate) fn error_message(message: &str) -> String {
    let mut output = String::new();
    for character in message.chars().take(1024) {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct BuildIdentity {
    core: String,
    revision: String,
    api: String,
    backend: String,
}

impl BuildIdentity {
    pub(super) fn current() -> Self {
        Self {
            core: prismattyc_core::package_version().into(),
            revision: prismattyc_core::git_hash().into(),
            api: pmux_plugin_api::API_VERSION.into(),
            backend: "pulley64".into(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Bootstrap {
    pub build: BuildIdentity,
    pub component: Vec<u8>,
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
pub(super) enum Operation {
    Validate {
        schemas: BTreeMap<String, CommandSchema>,
    },
    Prepare {
        context: lifecycle::Context,
        assets: BTreeMap<String, Vec<u8>>,
        schemas: BTreeMap<String, CommandSchema>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct CommandSchema {
    pub input: String,
    pub output: String,
}

#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Bootstrap(Bootstrap),
    Activate,
    Invoke(lifecycle::Message),
    Stop,
    Endpoints(Result<Vec<read::Endpoint>, read::Error>),
    Page(Result<ActivityPage, read::Error>),
}

#[derive(Serialize, Deserialize)]
pub(super) enum Response {
    Validated,
    Prepared,
    Activated,
    Reply(lifecycle::Reply),
    Stopped,
    Endpoints,
    Poll {
        endpoint: u64,
        after: Option<ActivityCursor>,
        limit: u16,
    },
    Failed(String),
    Rejected,
}

pub(super) fn write<T: Serialize>(output: &mut impl Write, value: &T) -> Result<()> {
    let bytes = postcard::to_allocvec(value)?;
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "worker message exceeds its byte budget"
    );
    output.write_all(&(bytes.len() as u32).to_le_bytes())?;
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}

pub(super) fn read<T: DeserializeOwned>(input: &mut impl Read) -> Result<T> {
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(
        length <= MAX_MESSAGE_BYTES,
        "worker message exceeds its byte budget"
    );
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    let (value, rest) = postcard::take_from_bytes(&bytes).context("invalid worker message")?;
    ensure!(rest.is_empty(), "trailing worker message bytes");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_activity_page_crosses_worker_wire_without_session_metadata() {
        let page: ActivityPage = serde_json::from_value(serde_json::json!({
            "epoch":"abc1", "oldest_sequence":1, "current_sequence":1,
            "records":[{"sequence":1,"received_at_ms":1000,"event":{
                "schema_version":1,"seat":"legacy-seat","run_id":"legacy-run",
                "event_id":"event-1","source":"hooks","kind":"waiting"
            }}],
            "cursor":{"epoch":"abc1","sequence":1},"has_more":false,"reset":null
        }))
        .unwrap();
        let mut bytes = Vec::new();
        write(&mut bytes, &Request::Page(Ok(page))).unwrap();
        let Request::Page(Ok(page)) = read::<Request>(&mut std::io::Cursor::new(bytes)).unwrap()
        else {
            panic!("expected an activity page");
        };
        assert_eq!(page.epoch, "abc1");
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].event.seat.as_str(), "legacy-seat");
        assert_eq!(page.cursor.sequence, 1);
        assert!(page.sessions.is_none());
    }

    #[test]
    fn current_session_states_cross_worker_wire_after_history_catch_up() {
        use prismattyc_core::activity::ActivitySessionState;
        let page: ActivityPage = serde_json::from_value(serde_json::json!({
            "epoch":"abc1", "oldest_sequence":9, "current_sequence":264,
            "records":(9..=264).map(|sequence| serde_json::json!({
                "sequence":sequence,"received_at_ms":1000,"event":{
                    "schema_version":1,"seat":"busy-seat","run_id":"busy-run",
                    "event_id":format!("event-{sequence}"),"source":"hooks","kind":"working"
                }
            })).collect::<Vec<_>>(),
            "cursor":{"epoch":"abc1","sequence":264},"has_more":false,"reset":null,
            "sessions":{"observed_at_ms":1000,"seats":[
                {"seat":"quiet-seat","session_id":1,"state":"idle","last_output_at_ms":null,
                    "harness":"codex","model":"o4-mini","quota":null,"project":"Prismattyc"},
                {"seat":"unhooked-seat","session_id":2,"state":"stuck","last_output_at_ms":null,
                    "harness":null,"model":null,"quota":null,"project":null}
            ]}
        }))
        .unwrap();
        let mut bytes = Vec::new();
        write(&mut bytes, &Request::Page(Ok(page))).unwrap();
        let Request::Page(Ok(page)) = read::<Request>(&mut std::io::Cursor::new(bytes)).unwrap()
        else {
            panic!("expected an activity page");
        };
        let sessions = page.sessions.unwrap();
        assert_eq!(page.records.len(), 256);
        assert_eq!(sessions.seats[0].state, ActivitySessionState::Idle);
        assert_eq!(sessions.seats[1].state, ActivitySessionState::Stuck);
        assert_eq!(sessions.seats[0].harness.as_deref(), Some("codex"));
        assert_eq!(sessions.seats[0].model.as_deref(), Some("o4-mini"));
        assert_eq!(sessions.seats[0].project.as_deref(), Some("Prismattyc"));
        assert!(sessions.seats[0].quota.is_none());
        assert!(sessions.seats[1].harness.is_none());
    }

    #[test]
    fn compiler_and_manifest_errors_cannot_emit_terminal_controls() {
        assert_eq!(
            error_message("invalid \x1b]52;c;data\x07\npattern"),
            "invalid \\u{1b}]52;c;data\\u{7}\\npattern"
        );
    }

    #[test]
    fn private_framing_rejects_oversized_truncated_and_trailing_messages() {
        assert!(read::<Request>(&mut u32::MAX.to_le_bytes().as_slice()).is_err());
        assert!(read::<Request>(&mut [1, 0, 0, 0].as_slice()).is_err());
        assert!(read::<Request>(&mut [2, 0, 0, 0, 1, 99].as_slice()).is_err());
        let mut bytes = Vec::new();
        write(&mut bytes, &Request::Activate).unwrap();
        assert_eq!(bytes, [1, 0, 0, 0, 1]);
        assert!(matches!(
            read::<Request>(&mut bytes.as_slice()).unwrap(),
            Request::Activate
        ));
    }
}
