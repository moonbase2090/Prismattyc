# Local agent activity feed

This spike observes headless seats through an opt-in pmuxd feed and a standalone
Lunatui terminal viewer. A seat needs an explicit logical identity and a local
socket. It does not need a Prismattyc Space or terminal pane.

The prototype answers whether a small local feed can connect harness events to
a live monitor. It is not an audit log or a durable task history.

## Ownership and transport

```mermaid
flowchart LR
    H[Lifecycle hook or turn notification] --> E[Metadata adapter]
    R[Explicit runtime event] --> E
    E -->|publish_activity| D[Local pmuxd]
    D --> B[Bounded activity ring]
    V[Lunatui viewer] -->|read_activity with cursor| D
    B --> V
```

`ControlPlane` owns the activity ring. Publishing never emits a topology event,
changes a session, or consumes mailbox messages. Activity has a separate sequence
so a busy producer cannot evict geometry or controller-lease events.

The two new requests reuse the existing same-user local control socket on Unix
and Windows. The daemon rejects both requests until started with
`--experimental-activity`. The viewer and adapter are development tools, with no
automatic installation or hook configuration changes.

A disabled feed returns the control error code `activity_disabled`, distinct
from `invalid_request`. The Rust client retains rejection codes in
`activity::ActivityRejected`, available through `anyhow::Error::downcast_ref`.
It omits daemon message text. A connector can map this code to `feed_disabled`
without parsing diagnostics or mistaking a malformed request for a missing capability.

Each host has its own pmuxd, socket, activity history, and random boot epoch.
The prototype reads one endpoint. Two hosts with the same seat name remain
independent. A future combined viewer needs explicit endpoint aliases, separate
cursors, and separate connection status. Sequence numbers from different daemons
have no global ordering. Remote transport and access policy need separate work.

## Event schema and replay

The publisher sends one versioned JSON request per line:

```json
{"type":"publish_activity","version":1,"request_id":1,"event":{"schema_version":1,"seat":"worker-a","run_id":"run-1","event_id":"event-1","source":"runtime","kind":"working"}}
```

The normalized event has these fields:

| Field | Meaning |
| --- | --- |
| `schema_version` | Activity schema version, currently `1`. |
| `seat` | Existing validated logical agent identity. No pane lookup. |
| `run_id` | Producer run or session correlation, 1 to 128 ASCII identifier characters. |
| `event_id` | Retry identity within that seat and run, with the same identifier limit. |
| `source` | `notify`, `hooks`, or `runtime`. |
| `kind` | Closed activity enum in `activity.rs`. |

Identifiers permit ASCII letters, digits, hyphens, underscores, periods, and
colons. Seat identities follow the existing mailbox identity format.
The daemon accepts no free-text summary, arguments, command text, prompts,
responses, file paths, file contents, or mail bodies. Adapters hash upstream
correlation identifiers before publication. Operators still choose neutral seat
identities. This format is data minimization, not authenticated seat attribution:
any client with same-user socket access can assert a seat.

Kinds cover session and turn start/end, tool/command/edit start/end/failure,
explicit test start/pass/failure, working/waiting/stopped state, and mail
sent/received metadata. `*_finished` means an operation ended. It does not assert
success. Test and mail events require an explicit producer; the spike does not
infer them from arbitrary command text or instrument the mailbox automatically.

The daemon adds `sequence` and `received_at_ms` to each record. A publish reply
returns an acknowledgement cursor and `duplicate`. The ring retains 512 records.
Retries with the same seat, run, and event identity return the original cursor
while retained. A retry with changed content fails. After eviction or restart,
the same identity can be accepted again. There is no durable exactly-once claim.

Readers send a null cursor for initial retained history:

```json
{"type":"read_activity","version":1,"request_id":1,"cursor":null,"limit":256}
```

An `ActivityPage` contains records, the boot epoch, oldest/current sequence,
`cursor`, `has_more`, and an optional reset reason. Limits are 1 through 256.
The returned cursor names the last delivered record, so paging does not skip
records. A cursor at `oldest_sequence - 1` is valid. An older cursor reports
`retention_gap`; another epoch reports `restarted`; an ahead cursor reports
`ahead_of_feed`. Reset pages return retained history successfully and leave
topology snapshot state intact. A restart loses the activity ring.

## Harness adapters

The adapter consumes only documented hook or notification fields. It never reads
transcripts, authentication files, or terminal output to reconstruct tool calls.

The [turn notification reference](https://learn.chatgpt.com/docs/config-file/config-advanced#notifications)
documents a JSON argument with `agent-turn-complete`, `thread-id`, and `turn-id`.
The `--notify` adapter emits `turn_finished` only. Completion notifications alone
cannot supply a tool-by-tool timeline.

The [lifecycle hook reference](https://learn.chatgpt.com/docs/hooks) documents JSON
on stdin, session and tool correlation, and `PreToolUse` and `PostToolUse`.
The `--hook` adapter maps shell calls to command activity, patch/write calls to
edit activity, and other tools to generic tool activity. Hosted tools can lack
these hooks. Hook definitions require trust review; availability must be checked
against the installed runtime rather than inferred from notification support.

The [alternate hook reference](https://code.claude.com/docs/en/hooks) also documents
stdin lifecycle hooks, including `PostToolUseFailure`. The same metadata adapter
can normalize that event to an explicit failure. Support for both input formats
does not establish end-to-end validation on both runtimes. The proof procedure
below records which real producer was exercised.

For a runtime without a verified hook API, emit explicit `runtime` events through
`--event`. A process wrapper can report session start/end and exit failure, but
cannot claim tool, edit, or test visibility. Discovery of that runtime's public
event interface remains a prerequisite for its full adapter.

Hooks and notifications wait only for bounded local socket operations. Publishing
failure prints a fixed diagnostic to stderr and returns success, so observation
cannot reject a tool. Nothing is written to stdout. This best-effort spike has
no spool or automatic retry. Lifecycle events without correlation receive new
event identities; correlated tool and notification events have stable identities.

## Run the prototype

Build the daemon and adapter from the repository root:

```sh
cargo build -p prismattyc-mux --bins --example activity-emit --locked
cargo build --manifest-path tools/activity-monitor/Cargo.toml --locked
```

Start a separate development daemon with a private absolute socket path:

```sh
target/debug/pmuxd --experimental-activity --socket /tmp/activity-demo/feed.sock -- /bin/sh
```

The parent directory must exist and be private to the user. Do not replace a
running daemon or point verification at an operator socket.

Start the viewer in another terminal:

```sh
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/feed.sock
```

Configure a reviewed lifecycle command to run the absolute adapter path with
`--socket PATH --seat worker-a --hook`. Configure a notification argv to end with
`--socket PATH --seat worker-a --notify`; the runtime appends its JSON argument.
Keep installation at the user/runtime configuration boundary. On Windows, use
absolute executable paths and a reviewed Windows command override where the
harness provides one. No configuration in this spike alters an existing seat.

The viewer polls every 250 ms on a background thread. A bounded channel separates
network reads from rendering, and the cache keeps at most 128 seats. Timer frames
make new events visible without keyboard input. Resets clear the cache before
replay. Seats without an event for 30 seconds display as stale, which does not
mean their processes died. Disconnection leaves cached rows visibly stale and
retries the endpoint. Press `q` to exit.

`--snapshot` renders one 120 by 24 Lunatui grid as text. `--json` reads one bounded
page. Both report pending history if the page does not contain the full buffer.
The standalone tool pins Lunatui v0.1.18 by commit and has its own lockfile, so
ordinary workspace builds do not acquire the viewer dependency.

## Bot access through Shell

A bot with Shell access to a host can query the same private socket without
opening a terminal UI. The monitor's connector modes emit JSON:

```sh
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/feed.sock --seats
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/feed.sock --recent worker-a
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/feed.sock --tail --seat worker-a
```

`--seats` lists seats observed in retained events. It does not assert that a
process is alive. `--recent` returns retained events for one validated seat.
Finite queries drain bounded pages rather than returning only the first page.
`--tail` emits cursor-bearing JSON lines with reset information and optional
seat filtering. Filtering must still advance the underlying cursor through
unmatched events. A consumer treats reset as lost continuity and never infers
successful work from an idle connection.

The bot runs its command on the machine owning that daemon. It supplies an
explicit endpoint and a neutral host label in its own result aggregation.
Shell access provides the existing local-user trust boundary. The spike adds
no network listener or credentials, and the connector does not publish events,
consume mail, or send terminal input.

A read-only MCP option can wrap this same bounded API with `activity_list_seats`,
`activity_recent`, and `activity_poll` tools. Each call returns a bounded JSON
result and an explicit cursor/reset. Prefer polling tools over an indefinitely
open tool call. The server needs an operator-configured endpoint allowlist,
page/rate limits, and host labels. It must not accept arbitrary socket paths
from tool arguments. MCP packaging and access policy are design work only in
this spike. The existing Shell connector supplies the usable integration now.

## Reproduce the proof

```sh
python3 scripts/verify-activity.py \
  --bins target/debug \
  --monitor tools/activity-monitor/target/debug/pmux-activity-monitor \
  --out /tmp/activity-proof-new
```

This Unix driver creates isolated configuration and a private daemon, opens a
real terminal viewer, and emits a hook event after the first frame. It requires
the new seat to appear without keyboard input. It also checks default-off
rejection, metadata omission, retry conflict, pagination, retention boundaries,
restart epochs, and unchanged topology. It retains a rendered grid, live terminal
text, and a JSON result, then reaps its direct child processes and removes scratch state.

To exercise a real producer, pass `--producer-command FILE`. The file is a vetted
JSON argv array. Its arguments can contain `{socket}`, `{emitter}`, and `{seat}`
placeholders. The driver runs it in a private working directory, then requires
an actual hook or notification event from that seat. Review the producer command
before running it. Runner authentication uses its normal environment. The proof
does not save raw producer output or configuration.

Shared transport code provides a Windows implementation path. This Unix proof
does not establish Windows runtime behavior. A Windows run, clean VM proof, and
the remaining adapter installations are still required before a production v1.

## Design choice and v1 estimate

Two designs were compared. The native ring keeps one state owner and two
operations. A durable sidecar journal still needs daemon routing, plus another
process, queue, file recovery, retention, and shutdown policy. The ring is the
better one-day spike. The viewer retains the sidecar design's background reader,
bounded cache, explicit resets, and live-terminal verification.

A local v1 with bounded, process-local retention is an estimated 6 to 9
engineering days after the spike:

| Work | Engineering days |
| --- | --- |
| Schema, capability reporting, retention and delivery policy | 1 to 2 |
| Reviewed harness adapters and installation procedures | 2 |
| Viewer navigation, JSON connector, reconnect and stale state | 1 to 2 |
| Mail/status producers, Windows packaging and failure proof | 2 to 3 |

This is a planning range, not a measurement. It assumes a confirmed event
interface for each runtime. Durable history adds a journal/recovery design.
Remote aggregation adds roughly 2 to 4 days after transport and access policy
are agreed. A packaged read-only MCP connector adds roughly 1 to 2 days for
endpoint policy, tool schemas, limits, and client proof. Authenticated per-seat
attribution and a new upstream runtime event API need separate estimates.
