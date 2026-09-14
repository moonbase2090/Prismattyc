# Send intentional text to an agent pane

Use `pmux pane-write` for direct collaboration in a recipient's terminal.
Use `pmux mail send` when you need stored messages and claim/commit tracking.
Both are supported collaboration paths. Choose the path for the task.

## Send text

1. Run `pmux ls` to find the recipient's pane ID.
2. Send the text to that exact pane:

   ```bash
   pmux pane-write 15 --text 'Review the parser change and report the failing case.' --json
   ```

3. Read the JSON receipt. `queued` means the PTY input queue accepted all
   bytes. It does not mean the agent accepted, read, or acted on the text.

For multiline text, read a file through stdin:

```bash
pmux pane-write 15 --stdin --json < review-request.txt
```

Text is literal UTF-8. The command does not decode backslash escapes.
It accepts 1 to 65,504 bytes. Tabs and newlines are allowed. Other control
characters are rejected. Use `pmux send` for raw control keys.

## Choose submission behavior

| `--submit` value | Behavior |
| --- | --- |
| `auto` (default) | Detect the foreground agent, paste the body, then send its submit sequence. |
| `enter` | Write the body followed by CR. Use this when you intentionally want Enter in the target application. |
| `none` | Write the body without adding a submit key. The input ledger remains dirty. |

Auto supports the Grok, Claude, Kiro, Codex, and Cursor agent families.
It uses bracketed paste for the body. Grok, Claude, and Kiro receive CR.
Codex receives two separate CR chunks. Cursor receives kitty Ctrl+Enter
(`ESC [ 13 ; 5 u`). Chunks are separated by 80 ms.
These are terminal adapters, not an application acknowledgement protocol.
Guest versions can change submission behavior.

Auto refuses an unknown foreground application. It does not turn a peer
message into a shell command as a fallback. Select `enter` or `none`
explicitly for other applications.

```bash
pmux pane-write 15 --text 'cargo check' --submit enter --json
pmux pane-write 15 --text 'draft text' --submit none --json
```

## Understand targeting and refusals

- The target is a pane ID. Session names and session IDs are not accepted.
- The CLI snapshots the pane's child PID. The server checks that PID before
  writing. A changed or exited child causes a refusal.
- A Space-bound protocol client must still target a pane in its bound Space.
- The operation writes to one pane. Sync-input does not copy it to siblings.
- The operation does not acquire, take over, or release controller leases.
  Another controller causes a refusal.
- Unsubmitted input causes a refusal, including for the current controller.
- Input within the previous 750 ms causes a refusal. At least 8 KiB of
  output within the previous three seconds also causes a refusal. These
  activity checks reduce interference; they cannot prove application readiness.
- The operation does not create mail, change mail depth, or fake a delivery ACK.

If the server refuses before queueing any bytes, correct the target or wait
until it is ready. Then send a new request. The CLI does not retry for you.

A `partial` receipt means some bytes were queued before the route failed or
filled. It includes accepted and total byte counts. No later chunk is sent
after that failure. Inspect the recipient before continuing. Do not replay
the full body automatically. A lost connection or reply is also ambiguous:
the server may already have queued input.

`--json` emits one JSON result on stdout. Command errors and partial writes
exit nonzero. Without `--json`, the command prints the queued byte counts.

## Use the socket protocol

Connect to the recipient daemon's Unix control socket. Send one JSON object
per line. Use protocol version 1 and positive, strictly increasing request
IDs on this connection. Register once:

```json
{"version":1,"request_id":1,"type":"register_client"}
```

Read `response.client_id` from the reply. Registration has no agent-name
field. This client ID is valid only on this connection.

Request a snapshot and select the exact pane plus its current `child_pid`:

```json
{"version":1,"request_id":2,"type":"snapshot"}
```

Send one operation. The IDs below are illustrative:

```json
{"version":1,"request_id":3,"type":"pane_write","client_id":7,"pane_id":15,"expected_child_pid":42000,"data":"Please review the parser change.","submit":"auto"}
```

`submit` defaults to `auto`. Do not acquire a lease as a prerequisite.
The server validates the identity, target, input state, and submission mode,
then queues the body and submit chunks while serializing control requests.
The chunks are not an atomic PTY transaction. Partial queueing is reported.

A complete success reply has this shape:

```json
{"version":1,"request_id":3,"status":"ok","response":{"kind":"pane_write_result","pane_id":15,"child_pid":42000,"nbytes":44,"total_bytes":44,"complete":true,"submit":"auto","error":null}}
```

Byte counts include paste framing and submit bytes. They count UTF-8 bytes,
not characters. A partial result uses `complete: false` and supplies `error`.
Check `complete` even when the outer status is `ok`.

| Error code | Meaning |
| --- | --- |
| `stale_id` | Client identity, pane, or expected child is no longer valid. |
| `not_controller` | Another client holds the controller lease. |
| `input_dirty` | The pane has unsubmitted input. |
| `input_busy` | Recent input or output crossed an activity threshold. |
| `invalid_request` | Invalid text or no supported agent for auto submission. |
| `backpressure` | The PTY input queue is full. |
| `input_route_unavailable` | The input route is unavailable or the pane left the client's Space. |

Match each reply to its request ID. On reconnect, register again and obtain
a fresh snapshot. Never borrow another connection's client ID.

This operation requires a daemon that supports `pane_write`. An older
daemon rejects it. The CLI does not silently fall back to raw writes.
Installing binaries does not upgrade an already running daemon.
