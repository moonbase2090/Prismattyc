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
pmux activity monitor [ARGS...]
```

`pmux activity monitor` opens the live activity monitor (`pmux-activity-monitor`,
from next to `pmux` or else `PATH`) with `--socket` set to the socket this
`pmux` resolved. Other arguments go to the monitor unchanged.

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

Each host has its own daemon and feed. There is no remote listener and no merge
of feeds. Sequence numbers and epochs from different hosts have no common
order, and the same seat name on two hosts is two seats. Keep one cursor per
host.

From a shell, run `pmux activity` on each host, for example over SSH, and label
the results:

```sh
pmux activity seats --host-label build-mac
ssh -T build-win pmux activity seats --host-label build-win
```

An MCP client can use one `pmux-mcp` with a hosts file instead (see below).

## MCP server

`pmux-mcp` serves the same reads as MCP tools:

| Tool | Arguments | Result |
| --- | --- | --- |
| `pmux_activity_hosts` | none | Configured host labels and their endpoint kind (`local` or `command`). Socket paths and commands are not shown. |
| `pmux_activity_seats` | `host` | The `seats` result for that host. |
| `pmux_activity_recent` | `seat`, `limit`, `host` | The `recent` result. |
| `pmux_activity_poll` | `cursor`, `seat`, `limit`, `host` | The `poll` result. Pass `next_cursor` back on the next call. |

`host` may be left out when only one host is configured. Results are the JSON
from `pmux activity` with the host label added. A failed call is an MCP error
result whose text is the same JSON error object.

There is no tail tool. A tool call that stays open holds the client and has
no natural end. `pmux_activity_poll` returns one bounded page and a cursor, so
the client controls the cadence.

### Read-only mode

```sh
pmux-mcp --activity-only [--activity-hosts FILE]
```

`--activity-only` serves only the four tools above. It needs no agent
identity, and it offers no mail, Space or pane tools, so an observer cannot
send, claim or type anything. Without `--activity-only`, `pmux-mcp` keeps its
usual identity and tools and adds the activity tools. `--supervise` restarts
the server in the same mode if it exits.

### Hosts file

Without `--activity-hosts`, the server reads one host, `local`: the pmuxd
named by `PMUX_SOCKET`, or the default socket. A hosts file names every host
the tools may read:

```toml
# Minimum time between two calls to the same host. Default 1000.
min_interval_ms = 1000

[[host]]
label = "build-mac"
socket = "default"            # this user's default pmuxd

[[host]]
label = "build-win"
command = ["ssh", "-T", "-o", "BatchMode=yes", "build-win", "pmux"]
```

Each `[[host]]` has a `label` (letters, digits, `-`, `_` or `.`, up to 64) and
exactly one of:

- `socket`: an absolute path to a local pmuxd socket, or `"default"`.
- `command`: an argument list that runs `pmux` on that host. The server
  appends `activity`, the query, and `--host-label LABEL`.

The file is operator configuration. Tool arguments only choose among its
labels. They never supply a path or a command. The server checks seats,
cursors and limits before it builds a command line, because a command endpoint
hands its arguments to a remote shell. A file may list up to 32 hosts.

Calls to one host are spaced by `min_interval_ms`. A call that comes too soon
fails with `rate_limited` and `retry_after_ms`. Hosts are limited separately.
A socket query times out after 10 seconds, a command query after 20.

### Reaching a Windows host

A command endpoint can reach a Windows host over SSH:

- The Windows host runs the OpenSSH server, and the SSH user is the user that
  runs pmuxd there. Each Windows user has their own pmuxd and socket.
- `pmux.exe` must be on that user's `PATH` in an SSH session, or the command
  must name its full path.
- Use key authentication and `BatchMode=yes`, so a password or host-key prompt
  fails at once instead of waiting out the timeout.
- pmuxd on that host must run with `--experimental-activity`.

The reverse works too: a client on Windows can run `pmux-mcp.exe` with a
hosts file whose command endpoint reaches the Mac over SSH.

### Client configuration

Configure the MCP client to start the server as a stdio command. On macOS or
Linux:

```json
{
  "command": "pmux-mcp",
  "args": ["--activity-only", "--activity-hosts", "/path/to/activity-hosts.toml"]
}
```

On Windows:

```json
{
  "command": "C:\\path\\to\\pmux-mcp.exe",
  "args": ["--activity-only", "--activity-hosts", "C:\\path\\to\\activity-hosts.toml"]
}
```

The surrounding keys depend on the client. Add `--supervise` for clients that
do not restart a stdio server that exits.

### Errors added by the server

Besides the `pmux activity` codes, a tool result can carry:

| Code | Meaning |
| --- | --- |
| `host_required` | Several hosts are configured and the call named none. |
| `unknown_host` | The label is not in the hosts file. |
| `rate_limited` | The previous call to this host was too recent. Wait `retry_after_ms`. |
| `host_unreachable` | The endpoint returned no JSON: SSH failed, `pmux` is missing or too old, or no answer arrived before the timeout. |

### Limits

- Windows: the connector and the server use the cross-platform local socket
  client and process handling, and the verification driver is written to run
  on Windows. It has not yet run there.
- Rate limits apply per server process. Two servers for the same host are
  limited separately.
- `daemon_unreachable` and `feed_changed_retry` are matched from fixed client
  error text. `feed_disabled` comes from a typed rejection.

## Verification

```sh
cargo test -p prismattyc-mux --lib activity_query
cargo test -p prismattyc-mux --bin pmux activity
cargo test -p pmux-mcp
cargo build -p prismattyc-mux --bins --example activity-emit -p pmux-mcp
python3 scripts/verify-activity-connector.py --bins target/debug --out NEW_DIR
```

The driver starts private daemons and checks every `pmux activity` command and
error, the MCP tools in both modes, a two-host hosts file with a command
endpoint, rate limiting, and an unreachable endpoint. It is written for Unix
and Windows; so far it has run on macOS. The feed's `scripts/verify-activity.py` exercises the activity
monitor's JSON modes, which use the same `activity_query` module.
