# Activity feed access for bots and scripts

`pmux activity` reads the [local agent activity feed](agent-activity-feed.md)
and prints JSON. It is for programs that reach a host through a shell, such as
a supervising bot that checks on headless seats. `pmux-mcp` offers the same
reads as MCP tools.

The connector is read-only. It does not publish events, consume mail, or send
terminal input. Both are experimental, like the feed itself.

## Prerequisites

The host's pmuxd must run with `--experimental-activity`, and something must
publish events, such as the harness adapters in the feed note. Each host has its
own daemon, socket, history, and boot epoch. `pmux activity` uses the same
socket selection as every other `pmux` command: `--socket`, `--instance`, or
the default. Run it as the user who owns that daemon.

## Commands

```sh
pmux activity seats
pmux activity recent SEAT [--limit N]
pmux activity poll [--cursor EPOCH:SEQ] [--seat SEAT] [--limit N]
pmux activity tail [--cursor EPOCH:SEQ | --from-oldest] [--seat SEAT] \
                   [--max-events N] [--max-seconds N]
```

Every command takes `--host-label LABEL`, which adds a `host` field to each JSON
object. The label is the caller's name for the host, so results from several
hosts stay apart once they are combined.

| Command | Output |
| --- | --- |
| `seats` | One object: `seats[]` with `seat`, `last_kind`, `last_source`, `last_sequence`, `last_received_at_ms`, `retained_events`, plus the feed `epoch` and `cursor`. A listed seat was seen in retained history. Its process may have exited. |
| `recent` | One object: `records[]` for one seat, oldest first. `--limit N` keeps the newest N and sets `truncated`. |
| `poll` | One object: `page` (an `ActivityPage`) and `next_cursor`. Without `--cursor` it starts at the oldest retained event. `--seat` filters records, but `next_cursor` still moves past other seats' events. |
| `tail` | One JSON line per change: `activity` pages and a `disconnected` notice per outage. When `--max-events` or `--max-seconds` is reached, a final `end` line carries `next_cursor`. Without `--cursor` it starts at the newest event, so it shows only new activity. `--from-oldest` replays retained history first. |

`seats` and `recent` drain retained history in at most two pages, up to the
newest event at the time of the first read. They report `feed_changed_retry`
instead of mixing histories when the daemon restarts mid-read.

Records carry only metadata: `seat`, `run_id`, `event_id`, `source`, `kind`,
`sequence` and `received_at_ms`. The feed never stores prompts, commands,
output, paths or mail bodies, so the connector cannot return them.

### Cursors

A cursor is `EPOCH:SEQUENCE`: the daemon's boot epoch and the last delivered
sequence. Pass `next_cursor` back unchanged. A page with a non-null `reset`
(`restarted`, `retention_gap`, `ahead_of_feed`) is still a successful page, but
continuity was lost. Drop cached seat state and rebuild it from that page.
Cursors from different hosts cannot be compared.

### Errors and exit status

Errors are JSON on stdout as well, so a caller only parses one stream:

```json
{"schema_version":1,"error":{"code":"feed_disabled","message":"activity request rejected (ActivityDisabled)"}}
```

| Code | Meaning | Exit |
| --- | --- | --- |
| `feed_disabled` | pmuxd runs without `--experimental-activity`. | 1 |
| `daemon_unreachable` | No daemon answers on the selected socket. | 1 |
| `feed_changed_retry` | The daemon restarted during a `seats` or `recent` read. Retry. | 1 |
| `request_rejected` | The daemon rejected the request for another reason. | 1 |
| `request_failed` | Any other failure, for example a malformed response. | 1 |
| `invalid_argument` | A bad option or value. Usage text goes to stderr. | 2 |

`tail` checks the feed once before it starts, so `feed_disabled` and
`daemon_unreachable` end it at once. A daemon that drops later produces one
`disconnected` line, and `tail` keeps retrying until a limit stops it.

## Following the feed from a bot

A bot that runs one shell command per step should keep a cursor for each host
and poll:

```sh
pmux activity seats --host-label build-mac          # overview
pmux activity poll --host-label build-mac           # first page, oldest first
pmux activity poll --cursor "$NEXT" --host-label build-mac
```

Repeat the last command with each `next_cursor` while `page.has_more` is true.
After that, poll at the cadence the bot needs. On a `reset`, discard what the
bot has cached for that host. To watch briefly for new activity, use a bounded
tail and keep its final cursor:

```sh
pmux activity tail --seat worker-a --max-events 5 --max-seconds 60
```

Always give a bound when a tool call must return. A tail with neither limit runs
until it is interrupted.

## Several hosts

Each host is queried separately, through a shell on that host. There is no
remote listener and no merge of feeds. The caller labels each result with
`--host-label` and keeps separate cursors. Sequence numbers and epochs from
different hosts have no common order. The same seat name on two hosts is two
seats.

The connector uses the cross-platform local socket client, so the same command
is expected to work against a Windows pmuxd. This spike has not run it on
Windows.

## MCP option

`pmux-mcp` adds three read-only tools: `pmux_activity_seats`,
`pmux_activity_recent` (`seat`, optional `limit`) and `pmux_activity_poll`
(optional `cursor`, `seat`, `limit`). Each runs `pmux activity` against the
socket the server was started with (`PMUX_SOCKET`, else the default), with a
10-second timeout. Tool results carry the same JSON. A nonzero exit becomes an
MCP error result that keeps the JSON error code.

Design choices:

- **No socket in tool arguments.** The operator picks the endpoint when
  configuring the server. A tool call cannot point the server at another socket.
- **Polling, not streaming.** There is no tail tool. A call that stays open
  holds the client and has no natural end. `pmux_activity_poll` returns one
  bounded page and a cursor, so the client controls cadence and stays
  stateless between calls.
- **One host per server.** An MCP client that watches several hosts runs one
  server per host. A shell-reachable host can run `pmux-mcp` over SSH.

Before this is more than a prototype:

| Work | Estimate |
| --- | --- |
| Endpoint allowlist: named hosts mapped to sockets in config, with the label added by the server | 0.5 day |
| Per-call page and rate limits, and a minimum poll interval | 0.5 day |
| Client setup notes for local and SSH-launched servers, and a client proof | 0.5 to 1 day |

That is 1.5 to 2 days on top of the feed's v1, in line with the feed note's
estimate for MCP packaging.

## Verification

```sh
cargo test -p prismattyc-mux --lib activity_query
cargo test -p prismattyc-mux --bin pmux activity
cargo test -p pmux-mcp
```

The feed's `scripts/verify-activity.py` exercises the activity monitor's JSON
modes, which now use the same `activity_query` module as `pmux activity`.
