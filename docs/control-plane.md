# Local mux control protocol

This reference describes the local socket protocol. Use [pmux commands](mux-cli.md)
for the command-line interface.

`prismattyc-mux::control` owns the display-free v0 protocol and reference server.
The transport is newline-delimited JSON over a Unix socket.

### Transport and trust boundary

- Protocol version is `1`; every request carries `version` and a non-zero
  `request_id`.
- Request IDs increase monotonically per connection. Replays or reordering are
  rejected as `stale_request_id`.
- The socket is chmod `0600`. On Linux, every accepted connection is also
  checked with `SO_PEERCRED` and must have the server's effective UID.
- The default path is `$XDG_RUNTIME_DIR/prismattyc/pmux.sock` (`pmux-<instance>.sock`
  for a named instance). Otherwise it is UID-qualified under `/tmp`.
- Existing live sockets, non-socket paths, and foreign-owned socket paths are
  never unlinked. Same-uid leftovers with no listener (`ECONNREFUSED` and
  kin) are **stale** and are replaced on bind. `probe_socket_liveness`
  classifies Missing / Live / Stale / Foreign for attach diagnosis.
- Requests, responses, event retention, client count, and structured spawn
  metadata are bounded. Each connection has an I/O timeout and its own thread;
  the domain mutex is released before writes, so a blocked observer cannot
  stall other clients or topology work.

### Snapshot and event replay

A snapshot contains the domain hierarchy, immutable layout tree, stable IDs,
pane metadata, and authoritative integer-cell geometry at sequence `S`.
Mutations append one event to a domain-wide monotonically increasing sequence.

Clients must take a new snapshot after each connection. They then request event
batches `after_sequence=S`. A batch reports:

- `through_sequence`: last event actually included;
- `current_sequence`: newest server event;
- `has_more`: whether another bounded batch is required.

If `after_sequence` is ahead of the server or older than the retained ring,
the server returns a structured error with `resnapshot_required=true`, the
oldest available sequence, and the current sequence. The connection must take
a fresh snapshot before reading events again.

### Mutations and spawning

v0 covers split, close, geometry resize, advisory focus-suggest, **named
session create/switch**, **window (tab) create/destroy/switch/rename**, **pane
move across windows**, coalesced **output-activity** events,
**SetPaneStatus**, and **server termination** via `ShutdownServer`.

Window verbs (`Window` is the tab per [mux architecture](architecture.md)):

- `CreateWindow { version, request_id, session_id, title, spawn, cols, rows }`
  allocates one leaf in an existing session and registers `WindowBounds`
  atomically (same pattern as `CreateSession`).
- `DestroyWindow { version, request_id, window_id }` wires
  `Domain::destroy_window` (empty-session policy applies).
- `MovePane { version, request_id, from_window_id, to_window_id, pane_id,
  target_pane_id, axis, ratio, client_id? }` is one two-window mutation.
  Success emits exactly one `PaneMoved` (both window ids, suggested source
  focus, geometry spanning both windows). Never a `PaneClosed`+`PaneSplit`
  pair. Optional-lease tier matches `Split`/`Close`. Protocol version stays
  `1`.
- `SwitchWindow { version, request_id, client_id, window_id }` is
  client-local view state, like `SwitchSession`; it does not mutate
  topology or create a server-side active tab.
- `DestroySession { version, request_id, session_id }` destroys one
  session and every window/pane/PTY it owns. The long-lived server
  stays up (unlike `ShutdownServer`). No lease. Emits
  `SessionDestroyed { session_id, name }`. CLI:
  `pmux --session NAME stop` or `pmux stop NAME`. Protocol
  version stays `1`.
- `RenameWindow { version, request_id, window_id, title }` sets
  the window title in place. Same validation as `CreateWindow` (trimmed,
  1..=64 bytes, no NUL). No lease. Emits `WindowRenamed { window_id,
  title }` and returns a `Window` response. Protocol version stays `1`.
- `SetSyncInput { version, request_id, client_id, window_id, enabled }`
  sets a per-window flag. When the flag is on, `WritePane` fans the same
  bytes to every pane in that window. Siblings do not need the caller's
  lease. The focused pane still requires the controller lease; observers
  are rejected. Emits `SyncInputChanged { window_id, enabled }`.
  `WindowSnapshot` carries `sync_input` (serde default false).
  Protocol version stays `1`.
- `SetPaneStatus { version, request_id, pane_id, text }` sets short
  guest status text for one pane. `text: None` or empty-after-trim
  clears it. Trim, reject more than 64 bytes, reject any control
  character (`char::is_control`). Invalid payloads return
  `InvalidRequest` and store nothing. No lease. Emits
  `PaneStatusChanged { pane_id, status }`. `PaneSnapshot` carries
  `status` (serde default / skip if none). Protocol version stays `1`.
- `RaiseAttention { version, request_id, client_id, pane_id, message }`
  stores a validated generic attention message without writing to the pane
  PTY. It emits `PaneAttention { pane_id, message }` and keeps the latest
  message in `PaneSnapshot.attention`. A successful `WritePane` to the pane
  clears that attention and emits `PaneAttentionCleared { pane_id }`.
  Protocol version stays `1`.

Split accepts only a structured `SpawnSpec { program, argv, cwd, env }`.
There is no shell command-string field. CWD must be absolute and strings,
counts, and aggregate metadata size are bounded.

`ShutdownServer { version, request_id, client_id }` requires a registered
`client_id` (same tier as session verbs; no pane lease). The server writes
`ShutdownAccepted` to that caller **before** teardown; that ordering is
contract. Semantics: stop accepting, drop live pane runtimes (children see
PTY close/HUP, same as a signal stop today), remove the socket, exit 0.
Per [session ownership](architecture.md) this is termination, not
detach. Other connected clients see only connection close; there is no new
event kind. Protocol version stays `1`. The control plane cannot exit the
process: the connection handler raises a shutdown signal after the response
is flushed, and the server process waits on that signal, then drops the
listener (existing stop flag + `Drop` join/unlink). Signal-based stop
remains a CLI fallback when the socket is not live, the verb fails or
times out, or the pid survives the grace window.

The control plane is deliberately display-free and does not create a
second winit path. `ControlPlane` is the topology/control reference owner;
binding its accepted mutations to live `prismattyc-host` PTY runtimes is a
host adapter step. The connection-bound controller extension is documented in
the connection-bound controller lease.

### Mail attention and gated inject

Mail attention is a **mux-client** verb (`client_id` registered; no pane
lease). “Operator-class” in this protocol means that mux `client_id`, not a
human operator.

Each pane keeps a `queue_rev` **watermark** that survives Clear (a
tombstone). Any Set or Clear whose rev is **older or equal** to that
watermark is a no-op (first write of a rev wins — including a Clear). A
**greater** rev applies: Set stores attention; `depth == 0` or
`MailAttentionClear { queue_rev }` drops attention and advances the
watermark. An empty-pane Clear still plants the watermark so a delayed
older Set cannot resurrect mail. Success emits `MailAttentionChanged`
(depth 0 = cleared, carries the acting `queue_rev`) and returns the stored
attention plus the pane input ledger. Snapshot carries `mail` + `ledger`
per pane so host chrome and the inject client share one bit. This plane
does not write a mail body into the pane and does not `TakeoverLease`.

Gated inject is mux-client `InjectMail { pane_id, queue_rev,
remaining_attempts }`. The inject actor is a mux client (often an agent
helper). It is not a human keystroke. The only payload is a **fixed
doorbell token** plus a **submit sequence** for the pane child. Never a
mail body. Classify the child from `bound_pid` then the pane process tree.
Same-step gates (all required or no write): MailAttention depth>0 for this
`queue_rev` and not stuck/exhausted; `AcquireLease` succeeds (else defer;
**never** `TakeoverLease`); pane not focused (else `pane.mail` only);
fewer than 8KiB of PTY output in the last 3s; input not dirty. After write, no
content-epoch bump ⇒ `stuck` (never retry this rev). Epoch bump ⇒ `wrote`
and hide 60s or until epoch moves again. Defer/skip outcomes are `Ok` so
the caller does not count them as attempts.

The inject ledger records `last_output_at_ms` (stamped on coalesced
`OutputActivity`), `controller_id`, `last_controller_write_at_ms`,
`last_write_ended_with_cr`, derived `dirty_input` (bytes since last CR),
and `focused` from `ReportFocus` (per-client, not Domain topology).
`SuggestFocus` may carry `reason=mail`; it stays advisory.

`output_activity` is coalesced to at most one ring entry per pane so a busy
PTY cannot evict topology events. Clients that need content use `ReadPane`
(text) or `ReadPaneStyled` (RLE style runs). `ReadPane` stays
text-only so dumps remain byte-stable. `ReadPaneStyled` accepts optional
`view_offset` (rows back from the live tail; absent = live). The response
reports effective `view_offset` and `max_view_scroll`. Protocol version
stays `1`; old servers ignore the request field and omit the response
fields.

`SubscribePane { client_id, pane_id, from_seq, timeout_ms }` streams
per-pane log frames on the same connection. The handler runs on
the client thread, like `MailWait`, and never parks while holding the
plane lock. Protocol version stays `1`. `from_seq` is "I have through
this seq". If `from_seq + 1` is older than the retained ring, the first
frame is a gap: `ReadPaneStyled` snapshot plus `through_seq` at the
current log head, then later frames are new events only. If the seq is
still in the ring, frames carry `PaneLogFrame` values (`Output`,
`Resize`, `Title`, `Cwd`, `Status`, `Attention`, `MailDepth`, `Exited`).
`timeout_ms == 0` returns one catch-up frame with `done: true`. A
positive timeout writes catch-up, then further frames until an idle
period of `timeout_ms` (capped like `MailWait`) or the pane is gone.
Each frame is one NDJSON `ControlResponse` with the same `request_id`.
The last frame has `done: true`. A dedicated subscribe socket is
rejected: one connection already owns a thread.

Recovery checkpoints do not run in request handlers. Maintenance captures
at most one changed pane per tick and releases the control lock between
panes. Ordinary output captures only events since the previous checkpoint.
A background worker appends these events in newline-terminated transactions.
Unchanged panes do not export or rewrite their screen history. Only one checkpoint is in flight, so a
slow disk cannot build an unbounded queue of snapshots. Failed writes are
rate-limited and retried. Shutdown waits for the worker before writing the
final snapshot.

Recovery format 2 stores a base snapshot followed by event transactions.
The checkpoint cadence remains two seconds. A journal that reaches 4 MiB
requests a fresh base on the next dirty checkpoint. A resize, changed pane
identity, or gap in the event ring also requires a fresh pane snapshot.
Each transaction includes the active pane list, so removed panes cannot
return during recovery. Atomic rename publishes a complete replacement base.
Recovery ignores an incomplete final transaction after a process crash.
Complete malformed transactions reject the file. Writes do not call `fsync`;
the checkpoint cadence is not a power-loss durability guarantee.

The reader accepts format 1 files from earlier releases. Earlier daemons
cannot read format 2 files. Downgrading loses saved terminal history; it
does not restore the old PTY processes.

The server is local same-user automation. [session ownership](architecture.md)
extends this exact transport with server-owned PTY/emulator lifetime; it does
not replace the v0 identity, lease, snapshot, event, or resync contracts.
