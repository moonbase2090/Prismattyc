// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;

use anyhow::{bail, ensure, Context, Result};
use jsonschema::{Draft, PatternOptions, Retrieve, Uri, Validator};
use serde_json::Value;

use super::wire::CommandSchema;

struct DenyRetriever;
impl Retrieve for DenyRetriever {
    fn retrieve(&self, _: &Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema references are not permitted".into())
    }
}

pub(super) struct Schemas(BTreeMap<String, (Validator, Validator)>);

impl Schemas {
    /// Schema compilation and validation run only in the killable native worker.
    pub(super) fn compile(schemas: BTreeMap<String, CommandSchema>) -> Result<Self> {
        ensure!(schemas.len() <= 64, "too many command schemas");
        let mut compiled = BTreeMap::new();
        for (id, schema) in schemas {
            compiled.insert(id, (compile(&schema.input)?, compile(&schema.output)?));
        }
        Ok(Self(compiled))
    }

    pub(super) fn input(&self, id: &str, json: &str) -> Result<()> {
        let (input, _) = self.0.get(id).context("unknown command contribution")?;
        ensure!(
            input.is_valid(&serde_json::from_str::<Value>(json)?),
            "command arguments do not match their schema"
        );
        Ok(())
    }

    pub(super) fn output(&self, id: &str, json: &str) -> Result<()> {
        let (_, output) = self.0.get(id).context("unknown command contribution")?;
        let value: Value = serde_json::from_str(json)?;
        if output.is_valid(&value) {
            return Ok(());
        }
        let detail = output
            .iter_errors(&value)
            .next()
            .map(|error| schema_mismatch(&error))
            .unwrap_or_else(|| "schema".to_string());
        bail!("command result does not match its schema: {detail}")
    }
}

fn schema_mismatch(error: &jsonschema::ValidationError<'_>) -> String {
    let mut best_len = 0usize;
    let mut best = String::new();
    fn walk(error: &jsonschema::ValidationError<'_>, best_len: &mut usize, best: &mut String) {
        let path = error.instance_path().as_str();
        let described = describe(error);
        if path.len() > *best_len || (path.len() == *best_len && described.len() > best.len()) {
            *best_len = path.len();
            *best = if path.is_empty() {
                described
            } else {
                format!("{path} {described}")
            };
        }
        let nested = match error.kind() {
            jsonschema::error::ValidationErrorKind::OneOfNotValid { context }
            | jsonschema::error::ValidationErrorKind::OneOfMultipleValid { context }
            | jsonschema::error::ValidationErrorKind::AnyOf { context } => context,
            _ => return,
        };
        for child in nested.iter().flatten() {
            walk(child, best_len, best);
        }
    }
    walk(error, &mut best_len, &mut best);
    best
}

fn describe(error: &jsonschema::ValidationError<'_>) -> String {
    let keyword = error.kind().keyword();
    match error.kind() {
        jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
            format!("{keyword} ({})", unexpected.join(", "))
        }
        jsonschema::error::ValidationErrorKind::Required { property } => {
            format!("{keyword} ({property})")
        }
        _ => keyword.to_string(),
    }
}

fn compile(text: &str) -> Result<Validator> {
    ensure!(
        text.len() <= 64 * 1024,
        "command schema exceeds its byte budget"
    );
    let schema: Value = serde_json::from_str(text)?;
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_pattern_options(
            PatternOptions::regex()
                .size_limit(1024 * 1024)
                .dfa_size_limit(1024 * 1024),
        )
        .with_retriever(DenyRetriever)
        .build(&schema)
        .context("invalid or unsupported command schema")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_validate_observable_arguments_and_reject_file_or_network_refs() {
        let schemas = Schemas::compile(BTreeMap::from([("read".into(), CommandSchema {
            input: r#"{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":256}},"required":["limit"],"additionalProperties":false}"#.into(),
            output: r#"{"type":"object","properties":{"ok":{"const":true}},"required":["ok"],"additionalProperties":false}"#.into(),
        })])).unwrap();
        schemas.input("read", r#"{"limit":3}"#).unwrap();
        assert!(schemas.input("read", r#"{"limit":257}"#).is_err());
        assert!(schemas
            .input("read", r#"{"limit":3,"shell":"run"}"#)
            .is_err());
        let mismatch = schemas.output("read", r#"{"ok":false}"#).unwrap_err();
        let mismatch = format!("{mismatch}");
        assert!(
            mismatch.contains("does not match its schema") && mismatch.contains("/ok"),
            "{mismatch}"
        );
        assert!(schemas.input("undeclared", "{}").is_err());
        assert!(compile(r#"{"$ref":"file:///etc/passwd"}"#).is_err());
        assert!(compile(r#"{"$ref":"https://example.invalid/schema"}"#).is_err());
        assert!(compile(r#"{"type":"string","pattern":"(a)\\1"}"#).is_err());
    }

    #[test]
    fn activity_poll_output_schema_accepts_a_bound_seat_page() {
        use prismattyc_core::activity::{
            ActivityCursor, ActivityDetail, ActivityInput, ActivityKind, ActivityPage,
            ActivityRecord, ActivitySession, ActivitySessionState, ActivitySessions,
            ActivitySource,
        };
        use prismattyc_core::AgentId;
        use serde_json::json;

        // The packaged connector schemas are the contract the daemon validates
        // against, so read them from the sibling crate instead of copying them.
        let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pmux-activity-connector-plugin/package");
        let output = std::fs::read_to_string(package.join("poll-output.json")).unwrap();
        let schemas = Schemas::compile(BTreeMap::from([(
            "poll".into(),
            CommandSchema {
                input: r#"{"type":"object"}"#.into(),
                output,
            },
        )]))
        .unwrap();

        // One seat per published session state, every optional field set, plus a
        // record carrying full event detail: the shape a bound seat produces.
        let seat = |name: &str, session_id: u64, state: ActivitySessionState| ActivitySession {
            seat: AgentId::new(name).unwrap(),
            session_id,
            state,
            last_output_at_ms: Some(9_000),
            harness: Some("codex".into()),
            model: Some("model-x".into()),
            quota: Some("80% left".into()),
            project: Some("demo".into()),
        };
        let page = ActivityPage {
            epoch: "a0".into(),
            oldest_sequence: 1,
            current_sequence: 1,
            records: vec![ActivityRecord {
                sequence: 1,
                received_at_ms: 9_000,
                event: ActivityInput {
                    schema_version: 1,
                    seat: AgentId::new("worker-a").unwrap(),
                    run_id: "run-1".into(),
                    event_id: "event-1".into(),
                    source: ActivitySource::Hooks,
                    kind: ActivityKind::ToolStarted,
                    process_ids: vec![42],
                    detail: Some(ActivityDetail {
                        tool: Some("cargo".into()),
                        file: Some("src/main.rs".into()),
                        command: Some("cargo test".into()),
                        duration_ms: Some(10),
                        exit_code: Some(0),
                        task: Some("ticket".into()),
                        operation_id: Some("op-1".into()),
                        model: Some("model-x".into()),
                    }),
                },
            }],
            cursor: ActivityCursor {
                epoch: "a0".into(),
                sequence: 1,
            },
            has_more: false,
            reset: None,
            sessions: Some(ActivitySessions {
                observed_at_ms: 9_500,
                seats: vec![
                    seat("worker-a", 1, ActivitySessionState::Working),
                    seat("worker-b", 2, ActivitySessionState::Waiting),
                    seat("worker-c", 3, ActivitySessionState::Idle),
                    seat("worker-d", 4, ActivitySessionState::Stuck),
                    seat("worker-e", 5, ActivitySessionState::Stopped),
                ],
            }),
        };
        for seat_filter in [json!("worker-a"), Value::Null] {
            let bound = json!({
                "schema_version": 1,
                "seat_filter": seat_filter,
                "next_cursor": "a0:1",
                "page": page,
            });
            schemas
                .output("poll", &bound.to_string())
                .unwrap_or_else(|e| panic!("bound-seat poll page rejected: {e:#}"));
        }

        // The schema stays closed: an undeclared seat field is a mismatch, and
        // the error names where.
        let mut rejected = json!({
            "schema_version": 1,
            "seat_filter": "worker-a",
            "next_cursor": "a0:1",
            "page": page,
        });
        rejected["page"]["sessions"]["seats"][0]["surprise"] = json!("x");
        let mismatch = format!(
            "{}",
            schemas.output("poll", &rejected.to_string()).unwrap_err()
        );
        assert!(mismatch.contains("does not match its schema"), "{mismatch}");
        assert!(mismatch.contains("/page/sessions/seats/0"), "{mismatch}");
    }
}
