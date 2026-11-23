// SPDX-License-Identifier: MIT

use pmux_plugin_api::guest::pmux::activity::read;
use prismattyc_core::{
    activity::{ActivityCursor, ActivityKind, ActivityPage},
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

    fn wait(
        &mut self,
        endpoint: u64,
        seat: &AgentId,
        kinds: &[ActivityKind],
        timeout_ms: u64,
    ) -> Result<ActivityPage, read::Error> {
        prismattyc_core::activity::wait_for_activity(
            |after| self.poll(endpoint, after.cloned(), 256),
            |slice| std::thread::sleep(std::time::Duration::from_millis(slice)),
            || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
                    .unwrap_or(0)
            },
            seat,
            kinds,
            timeout_ms.min(prismattyc_core::activity::PLUGIN_ACTIVITY_WAIT_MS),
        )
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    host: Option<String>,
    seat: Option<String>,
    cursor: Option<String>,
    since_ms: Option<u64>,
    limit: Option<u16>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

struct Failure {
    code: &'static str,
    message: String,
    retry_after_ms: Option<u32>,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
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
            message: message.to_string(),
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
        if !matches!(contribution, "hosts" | "seats" | "recent" | "poll" | "wait") {
            return Err(Failure::new("invalid_argument", "unknown activity command"));
        }
        let arrival_args = args.kinds.is_some() || args.timeout_ms.is_some();
        if (contribution == "hosts"
            && (args.host.is_some()
                || args.seat.is_some()
                || args.cursor.is_some()
                || args.since_ms.is_some()
                || args.limit.is_some()
                || arrival_args))
            || (contribution == "seats"
                && (args.seat.is_some()
                    || args.cursor.is_some()
                    || args.since_ms.is_some()
                    || args.limit.is_some()
                    || arrival_args))
            || (contribution == "recent"
                && (args.cursor.is_some() || args.since_ms.is_some() || arrival_args))
            || (contribution == "poll" && arrival_args)
            || (contribution == "wait"
                && (args.cursor.is_some() || args.since_ms.is_some() || args.limit.is_some()))
            || (args.cursor.is_some() && args.since_ms.is_some())
            || args.limit.is_some_and(|limit| !(1..=256).contains(&limit))
            || args
                .timeout_ms
                .is_some_and(|timeout| timeout > prismattyc_core::activity::MAX_ACTIVITY_WAIT_MS)
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
        if matches!(contribution, "recent" | "wait") && seat.is_none() {
            return Err(Failure::new(
                "invalid_argument",
                "this command requires a seat",
            ));
        }
        let kinds = if contribution == "wait" {
            let names = args
                .kinds
                .as_ref()
                .filter(|kinds| !kinds.is_empty())
                .ok_or_else(|| {
                    Failure::new("invalid_argument", "wait requires at least one event kind")
                })?;
            if args.timeout_ms.is_none() {
                return Err(Failure::new("invalid_argument", "wait requires a timeout"));
            }
            names
                .iter()
                .map(|kind| {
                    serde_json::from_value(json!(kind)).map_err(|_| {
                        Failure::new("invalid_argument", "unknown activity event kind")
                    })
                })
                .collect::<Result<Vec<ActivityKind>, _>>()?
        } else {
            Vec::new()
        };
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
        let handle = endpoint.handle;
        let interval = endpoint.min_interval_ms;
        if contribution == "wait" {
            let page = metadata
                .wait(
                    handle,
                    seat.as_ref().expect("wait seat was checked"),
                    &kinds,
                    args.timeout_ms.expect("wait timeout was checked"),
                )
                .map_err(|error| Failure::read(error, interval))?;
            let timed_out = page.records.is_empty();
            let mut value = query::poll_result(page, seat.as_ref());
            value["timed_out"] = json!(timed_out);
            value["host"] = json!(endpoint.label);
            return Ok(value);
        }
        let mut failure = None;
        let mut read_page = |cursor: Option<ActivityCursor>, limit: u16| {
            metadata.poll(handle, cursor, limit).map_err(|error| {
                failure = Some(Failure::read(error, interval));
                anyhow::anyhow!("activity page read failed")
            })
        };
        let mut value = if contribution == "poll" && cursor.is_none() && args.since_ms.is_none() {
            let page =
                query::latest_page(&mut read_page, args.limit.unwrap_or(256)).map_err(|error| {
                    failure
                        .take()
                        .unwrap_or_else(|| Failure::new("feed_changed_retry", error.to_string()))
                })?;
            query::poll_result(page, seat.as_ref())
        } else if contribution == "poll" && args.since_ms.is_none() {
            let page = read_page(cursor, args.limit.unwrap_or(256)).map_err(|error| {
                failure
                    .take()
                    .unwrap_or_else(|| Failure::new("feed_changed_retry", error.to_string()))
            })?;
            query::poll_result(page, seat.as_ref())
        } else {
            let history = query::retained(|cursor| read_page(cursor, 256)).map_err(|error| {
                failure
                    .take()
                    .unwrap_or_else(|| Failure::new("feed_changed_retry", error.to_string()))
            })?;
            if contribution == "poll" {
                let page = query::since_time(
                    &history,
                    args.since_ms.expect("time-based poll was selected"),
                    args.limit.unwrap_or(256).into(),
                );
                query::poll_result(page, seat.as_ref())
            } else if contribution == "seats" {
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
        kind: &'static str,
        slept: u64,
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
                    "schema_version":1,"seat":"worker-a","run_id":"run-1","event_id":"event-7","source":"notify","kind":self.kind
                }}],"cursor":{"epoch":"a0","sequence":7},"has_more":false,"reset":null
            })).unwrap())
        }

        fn wait(
            &mut self,
            endpoint: u64,
            seat: &AgentId,
            kinds: &[ActivityKind],
            timeout_ms: u64,
        ) -> Result<ActivityPage, read::Error> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            let clock = std::cell::Cell::new(0u64);
            let slept = std::cell::Cell::new(0u64);
            let page = prismattyc_core::activity::wait_for_activity(
                |after| self.poll(endpoint, after.cloned(), 256),
                |slice| {
                    slept.set(slept.get() + slice);
                    clock.set(clock.get() + slice);
                },
                || clock.get(),
                seat,
                kinds,
                timeout_ms,
            )?;
            self.slept = slept.get();
            Ok(page)
        }
    }

    fn fixture(kind: &'static str) -> Fixture {
        Fixture {
            labels: vec!["local"],
            calls: 0,
            failure: None,
            kind,
            slept: 0,
        }
    }

    #[test]
    fn hosts_disclose_kinds_and_policy_without_reading_activity() {
        let mut fixture = Fixture {
            labels: vec!["local", "remote"],
            calls: 0,
            failure: None,
            kind: "test_failed",
            slept: 0,
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
            kind: "test_failed",
            slept: 0,
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
            kind: "test_failed",
            slept: 0,
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
            kind: "test_failed",
            slept: 0,
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

    struct Window {
        calls: Vec<Option<u64>>,
    }
    impl Metadata for Window {
        fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
            Ok(vec![read::Endpoint {
                handle: 1,
                label: "local".into(),
                kind: read::EndpointKind::Local,
                min_interval_ms: 250,
            }])
        }
        fn poll(
            &mut self,
            _: u64,
            cursor: Option<ActivityCursor>,
            limit: u16,
        ) -> Result<ActivityPage, read::Error> {
            self.calls
                .push(cursor.as_ref().map(|cursor| cursor.sequence));
            let start = cursor.as_ref().map_or(0, |cursor| cursor.sequence);
            let sequences = (1..=5u64)
                .filter(|sequence| *sequence > start)
                .take(usize::from(limit))
                .collect::<Vec<_>>();
            let last = sequences.last().copied().unwrap_or(start);
            Ok(serde_json::from_value(json!({
                "epoch":"a0","oldest_sequence":1,"current_sequence":5,
                "records": sequences.iter().map(|sequence| json!({
                    "sequence": sequence, "received_at_ms": 1000,
                    "event": {"schema_version":1,"seat":"worker-a","run_id":"run-1","event_id":format!("event-{sequence}"),"source":"runtime","kind":"waiting"}
                })).collect::<Vec<_>>(),
                "cursor":{"epoch":"a0","sequence":last},
                "has_more": last < 5,
                "reset": null,
                "sessions": {"observed_at_ms": if cursor.is_none() { 1 } else { 2 }, "seats": [{
                    "seat":"worker-a","session_id":1,"state":"idle","last_output_at_ms":null,
                    "harness":null,"model":null,"quota":null,"project":null
                }]}
            }))
            .unwrap())
        }
    }

    #[test]
    fn cursorless_poll_returns_the_newest_window_when_history_spans_three_pages() {
        let mut window = Window { calls: Vec::new() };
        let (value, failed) = execute(&mut window, "poll", r#"{"limit":2}"#);
        assert!(!failed, "{value}");
        assert_eq!(value["page"]["records"][0]["sequence"], 4);
        assert_eq!(value["page"]["records"][1]["sequence"], 5);
        assert_eq!(value["page"]["sessions"]["observed_at_ms"], 2);
        assert_eq!(value["page"]["sessions"]["seats"][0]["state"], "idle");
        assert!(window.calls.len() >= 2, "{:?}", window.calls);
        assert!(window.calls[1].is_some_and(|sequence| sequence >= 3));
    }

    struct ShortPages {
        calls: usize,
    }
    impl Metadata for ShortPages {
        fn endpoints(&mut self) -> Result<Vec<read::Endpoint>, read::Error> {
            Ok(vec![read::Endpoint {
                handle: 1,
                label: "local".into(),
                kind: read::EndpointKind::Local,
                min_interval_ms: 250,
            }])
        }
        fn poll(
            &mut self,
            _: u64,
            _: Option<ActivityCursor>,
            _: u16,
        ) -> Result<ActivityPage, read::Error> {
            self.calls += 1;
            let sequence = self.calls as u64;
            Ok(serde_json::from_value(json!({
                "epoch":"a0","oldest_sequence":1,"current_sequence":10,
                "records":[{"sequence":sequence,"received_at_ms":1000,"event":{
                    "schema_version":1,"seat":"worker-a","run_id":"run-1","event_id":format!("event-{sequence}"),"source":"runtime","kind":"waiting"
                }}],
                "cursor":{"epoch":"a0","sequence":sequence},
                "has_more": true,
                "reset": null
            }))
            .unwrap())
        }
    }

    #[test]
    fn a_query_that_still_cannot_finish_names_the_reason() {
        let mut pages = ShortPages { calls: 0 };
        let (value, failed) = execute(&mut pages, "recent", r#"{"seat":"worker-a"}"#);
        assert!(failed);
        assert_eq!(
            value["error"]["message"],
            "activity feed exceeded the bounded query page limit"
        );
        assert_eq!(pages.calls, 2);
    }

    #[test]
    fn wait_returns_a_queued_match_immediately_and_bounds_an_empty_feed() {
        let mut queued = fixture("mail_received");
        let (value, failed) = execute(
            &mut queued,
            "wait",
            r#"{"seat":"worker-a","kinds":["mail_received"],"timeout_ms":60000}"#,
        );
        assert!(!failed, "{value}");
        assert_eq!(value["timed_out"], false);
        assert_eq!(
            value["page"]["records"][0]["event"]["kind"],
            "mail_received"
        );
        assert_eq!(value["page"]["records"][0]["event"]["event_id"], "event-7");
        assert_eq!(queued.calls, 1);
        assert_eq!(queued.slept, 0);
        let encoded = value.to_string();
        assert!(!encoded.contains("secret-subject"));
        assert!(!encoded.contains("secret-body"));

        let mut empty = fixture("working");
        let (value, failed) = execute(
            &mut empty,
            "wait",
            r#"{"seat":"worker-a","kinds":["room_message"],"timeout_ms":200}"#,
        );
        assert!(!failed, "{value}");
        assert_eq!(value["timed_out"], true);
        assert!(value["page"]["records"].as_array().unwrap().is_empty());
        assert!(empty.calls > 1);
        assert!(empty.slept <= 200, "{}", empty.slept);
        assert!(empty.slept > 0);

        let mut denied = fixture("mail_received");
        denied.failure = Some(read::Error::PermissionDenied);
        let (value, failed) = execute(
            &mut denied,
            "wait",
            r#"{"seat":"worker-a","kinds":["mail_received"],"timeout_ms":1000}"#,
        );
        assert!(failed);
        assert_eq!(value["error"]["code"], "request_rejected");
        assert_eq!(denied.calls, 0);
        assert!(value.to_string().contains("permission"));
        assert!(!value.to_string().contains("event-7"));
    }

    #[test]
    fn wait_rejects_an_unbounded_timeout_without_reading() {
        let mut fixture = fixture("mail_received");
        let (value, failed) = execute(
            &mut fixture,
            "wait",
            r#"{"seat":"worker-a","kinds":["mail_received"],"timeout_ms":60001}"#,
        );
        assert!(failed);
        assert_eq!(value["error"]["code"], "invalid_argument");
        assert_eq!(fixture.calls, 0);
    }
}
