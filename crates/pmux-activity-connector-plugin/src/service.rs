// SPDX-License-Identifier: MIT

use pmux_plugin_api::guest::pmux::activity::read;
use prismattyc_core::{
    activity::{ActivityCursor, ActivityPage},
    AgentId,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::query;

pub trait Metadata {
    fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error>;
    fn poll(
        &mut self,
        endpoint: u64,
        cursor: Option<ActivityCursor>,
        limit: u16,
    ) -> Result<ActivityPage, read::Error>;
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    host: Option<String>,
    seat: Option<String>,
    cursor: Option<String>,
    limit: Option<u16>,
}

struct Failure {
    code: &'static str,
    message: &'static str,
    retry_after_ms: Option<u32>,
}

impl Failure {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            retry_after_ms: None,
        }
    }

    fn read(error: read::Error, interval: u32) -> Self {
        let (code, message) = match error {
            read::Error::FeedDisabled => ("feed_disabled", "the activity feed is disabled"),
            read::Error::Unreachable => {
                ("host_unreachable", "the activity endpoint is unreachable")
            }
            read::Error::Connecting => (
                "host_unreachable",
                "the activity endpoint is connecting; retry",
            ),
            read::Error::UnknownEndpoint => {
                ("unknown_host", "the activity endpoint no longer exists")
            }
            read::Error::RateLimited => ("rate_limited", "wait before reading this endpoint again"),
            read::Error::Interrupted => ("feed_changed_retry", "the activity feed changed; retry"),
            read::Error::InvalidRequest => (
                "request_rejected",
                "the activity endpoint rejected the request",
            ),
            read::Error::IncompatibleSchema => (
                "request_failed",
                "the activity endpoint uses an incompatible schema",
            ),
            read::Error::NotActivated | read::Error::PermissionDenied | read::Error::Revoked => {
                ("request_rejected", "activity permission is unavailable")
            }
        };
        Self {
            code,
            message,
            retry_after_ms: matches!(error, read::Error::RateLimited).then_some(interval),
        }
    }

    fn json(self, host: Option<&str>) -> Value {
        let mut error = json!({"code":self.code,"message":self.message});
        if let Some(ms) = self.retry_after_ms {
            error["retry_after_ms"] = json!(ms);
        }
        let mut result = json!({"schema_version":1,"error":error});
        if let Some(host) = host {
            result["host"] = json!(host);
        }
        result
    }
}

pub fn execute(metadata: &mut impl Metadata, contribution: &str, arguments: &str) -> (Value, bool) {
    let mut host = None;
    let result = (|| {
        let args: Arguments = serde_json::from_str(arguments).map_err(|_| {
            Failure::new("invalid_argument", "expected an activity argument object")
        })?;
        host = args.host.clone();
        if !matches!(contribution, "hosts" | "seats" | "recent" | "poll") {
            return Err(Failure::new("invalid_argument", "unknown activity command"));
        }
        if (contribution == "hosts"
            && (args.host.is_some()
                || args.seat.is_some()
                || args.cursor.is_some()
                || args.limit.is_some()))
            || (contribution == "seats"
                && (args.seat.is_some() || args.cursor.is_some() || args.limit.is_some()))
            || (contribution == "recent" && args.cursor.is_some())
            || args.limit.is_some_and(|limit| !(1..=256).contains(&limit))
        {
            return Err(Failure::new(
                "invalid_argument",
                "invalid arguments for this activity command",
            ));
        }
        let seat = args
            .seat
            .as_deref()
            .map(AgentId::new)
            .transpose()
            .map_err(|_| {
                Failure::new("invalid_argument", "seat must be a valid agent identifier")
            })?;
        if contribution == "recent" && seat.is_none() {
            return Err(Failure::new("invalid_argument", "recent requires a seat"));
        }
        let cursor = args
            .cursor
            .as_deref()
            .map(query::parse_cursor)
            .transpose()
            .map_err(|_| {
                Failure::new(
                    "invalid_argument",
                    "cursor must be a hexadecimal epoch and sequence",
                )
            })?;
        let endpoints = metadata
            .endpoints()
            .map_err(|error| Failure::read(error, 1000))?;
        if contribution == "hosts" {
            let interval = endpoints
                .iter()
                .map(|endpoint| endpoint.min_interval_ms)
                .max()
                .unwrap_or(250);
            let hosts = endpoints.iter().map(|endpoint| json!({
                "label":endpoint.label,
                "endpoint": match endpoint.kind { read::EndpointKind::Command => "command", read::EndpointKind::Local | read::EndpointKind::Socket => "local" }
            })).collect::<Vec<_>>();
            return Ok(json!({"schema_version":1,"min_interval_ms":interval,"hosts":hosts}));
        }
        let endpoint = if let Some(label) = &args.host {
            endpoints
                .iter()
                .find(|endpoint| &endpoint.label == label)
                .ok_or_else(|| Failure::new("unknown_host", "unknown activity host"))?
        } else {
            match endpoints.as_slice() {
                [endpoint] => endpoint,
                [] => {
                    return Err(Failure::new(
                        "request_rejected",
                        "no activity endpoints are granted",
                    ))
                }
                _ => return Err(Failure::new("host_required", "select an activity host")),
            }
        };
        host = Some(endpoint.label.clone());
        let mut value = if contribution == "poll" {
            let page = metadata
                .poll(endpoint.handle, cursor, args.limit.unwrap_or(256))
                .map_err(|error| Failure::read(error, endpoint.min_interval_ms))?;
            query::poll_result(page, seat.as_ref())
        } else {
            let mut failure = None;
            let history = query::retained(|cursor| {
                metadata
                    .poll(endpoint.handle, cursor, 256)
                    .map_err(|error| {
                        failure = Some(Failure::read(error, endpoint.min_interval_ms));
                        anyhow::anyhow!("activity page read failed")
                    })
            })
            .map_err(|_| {
                failure.unwrap_or_else(|| {
                    Failure::new(
                        "feed_changed_retry",
                        "cannot read a consistent retained activity history; retry",
                    )
                })
            })?;
            if contribution == "seats" {
                query::observed_seats(&history)
            } else {
                query::recent_events(
                    &history,
                    seat.as_ref().expect("recent seat was checked"),
                    args.limit.map(usize::from),
                )
            }
        };
        value["host"] = json!(endpoint.label);
        Ok(value)
    })();
    match result {
        Ok(value) => (value, false),
        Err(error) => (error.json(host.as_deref()), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        labels: Vec<&'static str>,
        calls: usize,
        failure: Option<read::Error>,
    }
    impl Metadata for Fixture {
        fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
            Ok(self
                .labels
                .iter()
                .enumerate()
                .map(|(index, label)| read::Endpoint {
                    handle: index as u64 + 1,
                    label: (*label).into(),
                    kind: if index == 0 {
                        read::EndpointKind::Local
                    } else {
                        read::EndpointKind::Command
                    },
                    min_interval_ms: 1000,
                })
                .collect())
        }
        fn poll(
            &mut self,
            endpoint: u64,
            _: Option<ActivityCursor>,
            _: u16,
        ) -> Result<ActivityPage, read::Error> {
            self.calls += 1;
            if let Some(error) = self.failure {
                return Err(error);
            }
            assert_eq!(endpoint, 1);
            Ok(serde_json::from_value(json!({
                "epoch":"a0", "oldest_sequence":1,"current_sequence":7,
                "records":[{"sequence":7,"received_at_ms":1200,"event":{
                    "schema_version":1,"seat":"worker-a","run_id":"run-1","event_id":"event-7","source":"hooks","kind":"test_failed"
                }}],"cursor":{"epoch":"a0","sequence":7},"has_more":false,"reset":null
            })).unwrap())
        }
    }

    #[test]
    fn hosts_disclose_kinds_and_policy_without_reading_activity() {
        let mut fixture = Fixture {
            labels: vec!["local", "remote"],
            calls: 0,
            failure: None,
        };
        assert_eq!(
            execute(&mut fixture, "hosts", "{}"),
            (
                json!({"schema_version":1,"min_interval_ms":1000,"hosts":[{"label":"local","endpoint":"local"},{"label":"remote","endpoint":"command"}]}),
                false
            )
        );
        let (error, failed) = execute(&mut fixture, "seats", "{}");
        assert!(failed);
        assert_eq!(error["error"]["code"], "host_required");
        assert_eq!(fixture.calls, 0);
    }

    #[test]
    fn named_poll_filters_records_without_losing_the_resume_cursor() {
        let mut fixture = Fixture {
            labels: vec!["local"],
            calls: 0,
            failure: None,
        };
        assert_eq!(
            execute(
                &mut fixture,
                "poll",
                r#"{"host":"local","seat":"other-seat"}"#
            ),
            (
                json!({
                    "schema_version":1,"host":"local","seat_filter":"other-seat","next_cursor":"a0:7",
                    "page":{"epoch":"a0","oldest_sequence":1,"current_sequence":7,"records":[],"cursor":{"epoch":"a0","sequence":7},"has_more":false,"reset":null,"sessions":null}
                }),
                false
            )
        );
        assert_eq!(fixture.calls, 1);
    }

    #[test]
    fn typed_rate_failure_preserves_the_public_error_contract() {
        let mut fixture = Fixture {
            labels: vec!["local"],
            calls: 0,
            failure: Some(read::Error::RateLimited),
        };
        assert_eq!(
            execute(&mut fixture, "recent", r#"{"seat":"worker-a"}"#),
            (
                json!({
                    "schema_version":1,"host":"local","error":{"code":"rate_limited","message":"wait before reading this endpoint again","retry_after_ms":1000}
                }),
                true
            )
        );
        assert_eq!(fixture.calls, 1);
    }

    #[test]
    fn invalid_arguments_never_read_an_endpoint() {
        let mut fixture = Fixture {
            labels: vec!["local"],
            calls: 0,
            failure: None,
        };
        for (command, args) in [
            ("recent", "{}"),
            ("poll", r#"{"cursor":"a;echo:1"}"#),
            ("poll", r#"{"seat":"bad--seat"}"#),
            ("poll", r#"{"limit":0}"#),
            ("hosts", r#"{"host":"local"}"#),
        ] {
            let (error, failed) = execute(&mut fixture, command, args);
            assert!(failed);
            assert_eq!(error["error"]["code"], "invalid_argument");
        }
        assert_eq!(fixture.calls, 0);
    }
}
