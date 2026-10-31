// SPDX-License-Identifier: MIT
//! `pmux activity`: read-only JSON access to the experimental activity feed,
//! for scripts and bots that reach this host through a shell.
use super::*;
use anyhow::anyhow;
use prismattyc_mux::activity::ActivityCursor;
use prismattyc_mux::activity_query::{self, TailLimits, MAX_PAGE, QUERY_SCHEMA_VERSION};
use prismattyc_mux::mailbox::AgentId;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const USAGE: &str = "\
pmux activity seats [--host-label LABEL]
pmux activity recent SEAT [--limit N] [--host-label LABEL]
pmux activity poll [--cursor EPOCH:SEQ] [--seat SEAT] [--limit N] [--host-label LABEL]
pmux activity tail [--cursor EPOCH:SEQ | --from-oldest] [--seat SEAT]
                   [--max-events N] [--max-seconds N] [--host-label LABEL]

Read the activity feed of this host's pmuxd (started with --experimental-activity).
Every result, including errors, is JSON on stdout: one object, or one per
line for tail.

  seats    seats seen in retained history (not proof a process is alive)
  recent   one seat's retained events, oldest first; --limit keeps the newest N
  poll     one page after --cursor (omit it to start at the oldest retained
           event); continue with the returned next_cursor
  tail     follow new events from now (--cursor resumes after a cursor,
           --from-oldest replays retained history first); stops after the
           page that reaches --max-events records or after --max-seconds,
           then prints {\"type\":\"end\"} with next_cursor. With neither
           limit, runs until interrupted.

--host-label adds a \"host\" field so results from several hosts stay apart.
A \"reset\" in a page means continuity was lost: discard cached seat state.
Exit status: 0 on success, 1 on a feed error, 2 on a usage error.";

const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq, Eq)]
enum Query {
    Help,
    Seats,
    Recent {
        seat: AgentId,
        limit: Option<usize>,
    },
    Poll {
        cursor: Option<ActivityCursor>,
        seat: Option<AgentId>,
        limit: usize,
    },
    Tail {
        start: TailStart,
        seat: Option<AgentId>,
        limits: TailLimits,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum TailStart {
    Now,
    Oldest,
    After(ActivityCursor),
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    query: Query,
    host: Option<String>,
}

fn host_label(text: &str) -> Result<String> {
    ensure!(
        !text.is_empty()
            && text.len() <= 64
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "--host-label must be 1 to 64 letters, digits, '-', '_' or '.'"
    );
    Ok(text.to_owned())
}

fn number(flag: &str, text: &str, max: u64) -> Result<u64> {
    let value: u64 = text
        .parse()
        .map_err(|_| anyhow!("{flag} needs a whole number"))?;
    ensure!((1..=max).contains(&value), "{flag} must be 1 to {max}");
    Ok(value)
}

fn parse(args: &[String]) -> Result<Request> {
    let Some((verb, rest)) = args.split_first() else {
        bail!("missing activity command");
    };
    if matches!(verb.as_str(), "-h" | "--help" | "help") {
        return Ok(Request {
            query: Query::Help,
            host: None,
        });
    }
    let mut host = None;
    let mut seat = None;
    let mut cursor = None;
    let mut limit = None;
    let mut max_events = None;
    let mut max_seconds = None;
    let mut from_oldest = false;
    let mut positional = Vec::new();
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        let mut value = |flag: &str| {
            rest.next()
                .ok_or_else(|| anyhow!("{flag} needs a value"))
                .map(String::as_str)
        };
        match arg.as_str() {
            "-h" | "--help" => {
                return Ok(Request {
                    query: Query::Help,
                    host: None,
                })
            }
            "--host-label" => host = Some(host_label(value("--host-label")?)?),
            "--seat" => seat = Some(AgentId::new(value("--seat")?)?),
            "--cursor" => cursor = Some(activity_query::parse_cursor(value("--cursor")?)?),
            "--limit" => limit = Some(number("--limit", value("--limit")?, MAX_PAGE as u64)?),
            "--max-events" => {
                max_events = Some(number("--max-events", value("--max-events")?, 100_000)?)
            }
            "--max-seconds" => {
                max_seconds = Some(number("--max-seconds", value("--max-seconds")?, 86_400)?)
            }
            "--from-oldest" => from_oldest = true,
            flag if flag.starts_with('-') => bail!("unknown option {flag}"),
            word => positional.push(word),
        }
    }
    let only = |allowed: &[&str]| -> Result<()> {
        let used = [
            ("--seat", seat.is_some()),
            ("--cursor", cursor.is_some()),
            ("--limit", limit.is_some()),
            ("--max-events", max_events.is_some()),
            ("--max-seconds", max_seconds.is_some()),
            ("--from-oldest", from_oldest),
        ];
        for (flag, present) in used {
            ensure!(
                !present || allowed.contains(&flag),
                "{verb} does not take {flag}"
            );
        }
        Ok(())
    };
    let query = match verb.as_str() {
        "seats" => {
            only(&[])?;
            ensure!(positional.is_empty(), "seats takes no arguments");
            Query::Seats
        }
        "recent" => {
            only(&["--limit"])?;
            let [name] = positional.as_slice() else {
                bail!("recent needs exactly one SEAT");
            };
            Query::Recent {
                seat: AgentId::new(*name)?,
                limit: limit.map(|n| n as usize),
            }
        }
        "poll" => {
            only(&["--seat", "--cursor", "--limit"])?;
            ensure!(positional.is_empty(), "poll takes no positional arguments");
            Query::Poll {
                cursor,
                seat,
                limit: limit.map_or(MAX_PAGE, |n| n as usize),
            }
        }
        "tail" => {
            only(&[
                "--seat",
                "--cursor",
                "--from-oldest",
                "--max-events",
                "--max-seconds",
            ])?;
            ensure!(positional.is_empty(), "tail takes no positional arguments");
            ensure!(
                !(from_oldest && cursor.is_some()),
                "tail takes --cursor or --from-oldest, not both"
            );
            let start = match cursor {
                Some(cursor) => TailStart::After(cursor),
                None if from_oldest => TailStart::Oldest,
                None => TailStart::Now,
            };
            Query::Tail {
                start,
                seat,
                limits: TailLimits {
                    max_events: max_events.map(|n| n as usize),
                    max_duration: max_seconds.map(Duration::from_secs),
                },
            }
        }
        other => bail!("unknown activity command {other}"),
    };
    Ok(Request { query, host })
}

fn labeled(mut value: Value, host: Option<&str>) -> Value {
    if let (Some(host), Some(object)) = (host, value.as_object_mut()) {
        object.insert("host".into(), json!(host));
    }
    value
}

fn print_line(out: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    out.flush()
}

fn error_json(code: &str, message: &str, host: Option<&str>) -> Value {
    labeled(
        json!({
            "schema_version": QUERY_SCHEMA_VERSION,
            "error": {"code": code, "message": message}
        }),
        host,
    )
}

fn query(socket: &Path, query: Query, host: Option<&str>) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let result = match query {
        Query::Help => unreachable!("help is handled before the query"),
        Query::Seats => {
            activity_query::read_retained(socket).map(|h| activity_query::observed_seats(&h))
        }
        Query::Recent { seat, limit } => activity_query::read_retained(socket)
            .map(|h| activity_query::recent_events(&h, &seat, limit)),
        Query::Poll {
            cursor,
            seat,
            limit,
        } => activity_query::read_page(socket, cursor, limit)
            .map(|page| activity_query::poll_result(page, seat.as_ref())),
        Query::Tail {
            start,
            seat,
            limits,
        } => {
            // Probe once so an unreachable daemon or a disabled feed fails
            // with an error object instead of a retry loop.
            let now = match activity_query::current_cursor(socket) {
                Ok(now) => now,
                Err(error) => match fail(&mut out, &error, host)? {},
            };
            let start = match start {
                TailStart::Now => Some(now),
                TailStart::Oldest => None,
                TailStart::After(cursor) => Some(cursor),
            };
            let stop = AtomicBool::new(false);
            return activity_query::tail(
                |cursor| activity_query::read_page(socket, cursor, MAX_PAGE),
                start,
                seat.as_ref(),
                limits,
                POLL_INTERVAL,
                &stop,
                |value| print_line(&mut out, &labeled(value.clone(), host)),
            );
        }
    };
    match result {
        Ok(value) => {
            print_line(&mut out, &labeled(value, host))?;
            Ok(())
        }
        Err(error) => fail(&mut out, &error, host).map(|_| ()),
    }
}

fn fail(
    out: &mut impl Write,
    error: &anyhow::Error,
    host: Option<&str>,
) -> Result<std::convert::Infallible> {
    let code = activity_query::error_code(error);
    print_line(out, &error_json(code, &error.to_string(), host))?;
    std::process::exit(1);
}

pub(super) fn run(paths: &Paths, args: Vec<String>) -> Result<()> {
    let request = match parse(&args) {
        Ok(request) => request,
        Err(error) => {
            let mut out = std::io::stdout().lock();
            print_line(
                &mut out,
                &error_json("invalid_argument", &error.to_string(), None),
            )?;
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if request.query == Query::Help {
        println!("{USAGE}");
        return Ok(());
    }
    query(&paths.socket, request.query, request.host.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn activity_poll_parses_cursor_seat_limit_and_host() -> Result<()> {
        let request = parse(&args(&[
            "poll",
            "--cursor",
            "ab12:7",
            "--seat",
            "worker-a",
            "--limit",
            "10",
            "--host-label",
            "mac-1",
        ]))?;
        assert_eq!(request.host.as_deref(), Some("mac-1"));
        let Query::Poll {
            cursor,
            seat,
            limit,
        } = request.query
        else {
            panic!("expected poll");
        };
        let cursor = cursor.expect("cursor");
        assert_eq!((cursor.epoch.as_str(), cursor.sequence), ("ab12", 7));
        assert_eq!(seat.as_ref().map(AgentId::as_str), Some("worker-a"));
        assert_eq!(limit, 10);
        Ok(())
    }

    #[test]
    fn activity_poll_defaults_to_the_full_page_from_the_oldest_event() -> Result<()> {
        let request = parse(&args(&["poll"]))?;
        assert_eq!(
            request.query,
            Query::Poll {
                cursor: None,
                seat: None,
                limit: MAX_PAGE
            }
        );
        Ok(())
    }

    #[test]
    fn activity_tail_limits_are_parsed() -> Result<()> {
        let request = parse(&args(&["tail", "--max-events", "5", "--max-seconds", "30"]))?;
        assert_eq!(
            request.query,
            Query::Tail {
                start: TailStart::Now,
                seat: None,
                limits: TailLimits {
                    max_events: Some(5),
                    max_duration: Some(Duration::from_secs(30)),
                },
            }
        );
        Ok(())
    }

    #[test]
    fn activity_tail_starts_now_unless_told_to_resume_or_replay() -> Result<()> {
        let start = |words: &[&str]| -> Result<TailStart> {
            match parse(&args(words))?.query {
                Query::Tail { start, .. } => Ok(start),
                other => bail!("expected tail, got {other:?}"),
            }
        };
        assert_eq!(start(&["tail"])?, TailStart::Now);
        assert_eq!(start(&["tail", "--from-oldest"])?, TailStart::Oldest);
        let TailStart::After(cursor) = start(&["tail", "--cursor", "ab:9"])? else {
            bail!("expected a resume cursor");
        };
        assert_eq!(cursor.sequence, 9);
        assert!(parse(&args(&["tail", "--cursor", "ab:9", "--from-oldest"])).is_err());
        assert!(parse(&args(&["poll", "--from-oldest"])).is_err());
        Ok(())
    }

    #[test]
    fn activity_rejects_flags_that_do_not_apply_to_the_command() {
        for bad in [
            &["seats", "--seat", "worker-a"][..],
            &["recent", "worker-a", "--cursor", "ab:1"],
            &["poll", "--max-events", "3"],
            &["tail", "--limit", "3"],
            &["recent"],
            &["recent", "a", "b"],
            &["poll", "--limit", "0"],
            &["poll", "--limit", "257"],
            &["poll", "--cursor", "nothex:1"],
            &["tail", "--max-seconds", "-1"],
            &["seats", "--host-label", "has space"],
            &["seats", "--bogus"],
            &["unknown"],
            &[],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn activity_host_label_is_added_to_objects_only() {
        let value = labeled(json!({"schema_version": 1}), Some("mac-1"));
        assert_eq!(value["host"], "mac-1");
        assert_eq!(labeled(json!([1]), Some("mac-1")), json!([1]));
        assert!(labeled(json!({}), None).get("host").is_none());
    }
}
