# Live JSON Event Stream and Structured Pane Queries

Status: spike findings and design note (target base `origin/main` at `05458628`). Recommends **GO** for build across two incremental pull requests.

## Summary and Recommendation

**Recommendation: GO.**

The pmux server (`pmuxd`) already owns an ordered control-plane event ring (`Event` in `crates/prismattyc-mux/src/control.rs`), captures child exit codes in `LivePane` (`crates/prismattyc-mux/src/live.rs`), and inspects cross-platform process trees in `crates/prismattyc-mux/src/procinfo.rs`. What is missing for automation and editor integrations is:

1. **A versioned, streaming NDJSON event endpoint and CLI** (`SubscribeEvents` / `pmux events --json`) extending the existing control-plane event ring rather than creating a second bus.
2. **First-class lifecycle events missing from the ring today:** `child_exited` (with `pid`, `exit_code`, and `signal` for every pane, not only seat-addressed panes) and `client_connected` / `client_disconnected`.
3. **Structured per-pane queries** (`InspectPane` / `pmux pane inspect --json` and `pmux pane list --json`) returning live process, PID, working directory, last exit status, and opt-in listening ports without scraping terminal text.

### Refined Build Estimate

Grounded in recent `pmuxd` and control-plane pull requests, split into two shippable PRs:

| Slice | Scope | Active seat work | Wall-clock (incl. review, CI, clean-VM/Docker proof) |
| --- | --- | --- | --- |
| **PR 1: Versioned event stream + lifecycle events + core pane inspect** | Submodule extraction from `control.rs`, `EventWatch` off-lock streaming (`SubscribeEvents`), bounded per-client buffer + `lagged` frame, `ChildExited` + `last_exit` persistence on `LivePane`, `ClientConnected`/`ClientDisconnected`, legacy `Events` v1 compatibility filter, `InspectPane` (process, PID, cwd, `last_exit`), `pmux events --json`, `pmux pane inspect --json`, `pmux pane list --json` | **6–8 agent-hours** | **1.5–2 days** |
| **PR 2: Cross-platform listening port discovery + opt-in port events + remote query** | On-demand listening TCP/UDP port enumeration in `procinfo.rs` (Linux `/proc`, macOS `libproc`, Windows `GetExtendedTcpTable`), bounded `--ports` on `InspectPane`, optional low-frequency off-lock `PanePortsChanged` when subscribed, SSH remote pass-through | **4–6 agent-hours** | **1.5–2 days** (includes Windows + Linux + macOS proofs) |
| **Total** | Full event stream + structured pane queries + ports | **10–14 agent-hours** | **3–4 days** |

PR 1 alone unblocks labeled CLI `wait` and structured pane inspection immediately; PR 2 can follow right behind without blocking downstream CLI work.

---

## Current State in `pmuxd`

Audit of `origin/main` (`05458628`):

- **Control-plane event ring (`crates/prismattyc-mux/src/control.rs`):**
  - Single monotonic `sequence: u64` stored in `VecDeque<EventEnvelope>` with `DEFAULT_EVENT_CAPACITY = 256` (`MAX_EVENT_CAPACITY = 4096`).
  - `OutputActivity { pane_id, revision, child_alive }` is coalesced to at most one ring entry per pane so busy PTYs cannot evict topology events.
  - Today `ControlRequest::Events` is poll-only and has no condvar watch; by contrast, `SubscribePane` (`PaneLogWatch`) and `MailWait` (`MailboxWatch`) already implement the exact off-lock, client-thread NDJSON streaming pattern with `control_peer_closed` disconnect detection.
  - `Event` is serialized with `#[serde(tag = "kind", rename_all = "snake_case")]` and **no** unknown-variant fallback. Older compiled attach/host binaries calling `ControlRequest::Events` will fail deserialization if the server injects unrecognized `Event` variants into a legacy `Events` batch.
- **Child exit status (`crates/prismattyc-mux/src/live.rs`):**
  - `LivePane` already captures `portable_pty::ExitStatus` (`exit_code()` and `signal()`) when `child_alive` transitions from `true` to `false`, and already appends `PaneEvent::Exited { code, signal }` to the per-pane `PaneLog`.
  - However, `LivePane` only emits a `CellExitNotice` when `hive_cell` is present; ordinary panes emit only `OutputActivity { child_alive: false }` to the control-plane event ring, losing the exit code and signal.
  - `LivePane` also does not retain a `last_exit` record on the pane for subsequent inspection queries.
  - On Unix, PTY reader EOF can occasionally arrive slightly before `try_wait()` returns `Some(status)` (yielding `exit_code: None, signal: None` if not re-polled).
- **Process inspection (`crates/prismattyc-mux/src/procinfo.rs`):**
  - Already implements cross-platform `pid_alive`, `cmdline`, `cwd_of`, `children_of`, `is_process_descendant`, `live_foreground_pid`, `live_foreground_command`, `terminal_foreground`, and batched `PaneProcesses::read(roots)` across Linux (`/proc`), macOS (`sysctl` / `libproc`), and Windows.
  - Socket/port discovery is not yet cross-platform: `listener_inode` and `pid_holds_socket` are currently Linux-only Unix-socket helpers.
- **Separate channels (link, do not duplicate):**
  - **Local agent activity feed** ([`docs/agent-activity-feed.md`](../agent-activity-feed.md)): harness-level turn/tool/edit metadata on its own sequence ring and `activity.read:local` plugin grant. Keep it strictly separate from mux topology/lifecycle events.
  - **Program resume metadata**: OSC 7577 / `procinfo` restart-recovery metadata is owned by the program-resume flow. `InspectPane` should read live process state from `procinfo.rs` (and expose optional resume metadata by reference if already stored on the pane, never duplicating parser logic).

---

## Event Mapping and Missing Variants

Extend the existing `ControlPlane` event sequence—**do not create a second event bus**.

### Existing `Event` Variants That Map Directly

| Automation need | Existing `Event` variant(s) | Notes |
| --- | --- | --- |
| Pane created | `SessionCreated`, `WindowCreated`, `PaneSplit` | Already carry `session_id`, `window_id`, `pane_id`/`new_pane_id`, `spawn` metadata (never terminal text) |
| Pane closed / moved | `PaneClosed`, `PaneMoved`, `WindowDestroyed`, `SessionDestroyed` | Already atomic (moves emit `PaneMoved`, never `Closed`+`Split`) |
| View / focus changed | `SessionSwitched`, `WindowSwitched`, `FocusReported` | Already present and carry `client_id`, `session_id`/`window_id`, `pane_id`; keep these rather than adding a duplicate `session_view_changed` |
| Pane metadata / liveness | `PaneRenamed`, `PaneStatusChanged`, `PaneIdle`, `OutputActivity`, `LeaseChanged`, `PaneAttention`, `PaneAttentionCleared` | `OutputActivity` remains coalesced; carries `revision` and `child_alive` only (no PTY bytes) |

### New `Event` Variants to Add

1. **`ChildExited`**
   ```json
   {
     "kind": "child_exited",
     "pane_id": 7,
     "pid": 48210,
     "exit_code": 0,
     "signal": null,
     "exited_at_ms": 1764180000123
   }
   ```
   - Emitted once per pane when `child_alive` flips `true -> false`, for **all** panes (not only seat-addressed panes).
   - `exit_code: Option<u32>` and `signal: Option<String>` stay nullable for the rare EOF-before-reap race; `LivePane` performs one short non-blocking follow-up `try_wait()` on the next drain tick before finalizing `last_exit` (bounded, never blocking the mux loop).
   - Stored on `LivePane` as `last_exit: Option<PaneExitRecord>` so `InspectPane` can return it after the event ages out of the ring.

2. **`ClientConnected` and `ClientDisconnected`**
   ```json
   { "kind": "client_connected", "client_id": 12 }
   { "kind": "client_disconnected", "client_id": 12, "reason": "eof" }
   ```
   - Emitted on `RegisterClient` and on `DisconnectClient` / socket drop cleanup.
   - Metadata only (`client_id`, disconnect `reason`: `"explicit" | "eof" | "timeout"`). Never includes credentials, peer PIDs beyond existing trust checks, or socket paths.

3. **`PanePortsChanged` (Phase 2, opt-in)**
   ```json
   {
     "kind": "pane_ports_changed",
     "pane_id": 7,
     "ports": [
       { "proto": "tcp", "addr": "127.0.0.1", "port": 3000, "pid": 48299, "process": "node" }
     ]
   }
   ```
   - Emitted **only** when at least one active subscriber has opted in (`include_ports: true`). Zero polling cost when no subscriber requests port events.
   - Emits on set diffs only (sorted, deduplicated, capped at 64 ports per pane).

---

## Protocol, Versioning, and Backpressure

### Why a New Subscribe Verb (`SubscribeEvents`) Instead of Overloading `Events`

1. **Backwards compatibility for older attach/host/MCP clients:**
   - Existing clients send `ControlRequest::Events { version: 1, ... }` and deserialize `Event` strictly.
   - Keep `ControlRequest::Events` filtering out post-v1 event kinds (`ChildExited`, `ClientConnected`, `ClientDisconnected`, `PanePortsChanged`) when called by legacy v1 pollers, while still advancing `through_sequence` / `current_sequence` monotonically so legacy cursor arithmetic stays valid.
   - New streaming consumers use `ControlRequest::SubscribeEvents` (still over the existing local socket and `PROTOCOL_VERSION = 1` request framing, matching how `SubscribePane` was added without breaking existing clients).
2. **Zero polling latency and off-lock delivery:**
   - Add an `EventWatch` (`Mutex<u64>` + `Condvar`, identical to `PaneLogWatch` / `MailboxWatch`) notified at the end of `ControlPlane::emit()`.
   - Serve `SubscribeEvents` on the client connection thread (never holding the `ControlPlane` mutex while waiting or writing to the socket).

### Wire Contract (`SubscribeEvents` and `pmux events --json`)

Request:

```json
{
  "type": "subscribe_events",
  "version": 1,
  "request_id": 2,
  "stream_version": 1,
  "after_sequence": null,
  "filters": {
    "kinds": ["pane_split", "pane_closed", "child_exited"],
    "session_id": null,
    "pane_id": null
  },
  "include_ports": false,
  "timeout_ms": 0
}
```

- `stream_version`: explicit schema version for the emitted event envelope (currently `1`). Unsupported versions fail fast with `InvalidRequest` listing supported versions.
- `after_sequence`: `null` starts live from `current_sequence`; a numeric cursor replays retained events after that sequence (or emits a `lagged` frame if older than `oldest_available_sequence()`).
- `timeout_ms`: `0` keeps the stream open until peer close or server shutdown (with periodic lightweight liveness checks via `control_peer_closed`, bounded by 50 ms wait slices just like `SubscribePane`); a positive value closes cleanly with `"done": true` after that idle duration.

Each emitted NDJSON line on `pmux events --json` is a self-describing versioned envelope:

```json
{
  "schema_version": 1,
  "boot_epoch": "e4f2c9a1...",
  "sequence": 42,
  "ts_ms": 1764180000123,
  "kind": "child_exited",
  "pane_id": 7,
  "pid": 48210,
  "exit_code": 0,
  "signal": null
}
```

- **Stability guarantee (v1):** Additive fields and new `kind` values may be added within `schema_version: 1`; consumers must ignore unknown fields and unknown `kind` values. Breaking field renames, removals, or type changes require bumping `stream_version` / `schema_version`.
- **`boot_epoch`:** Random per-daemon-run token (same convention as the activity feed in [`docs/agent-activity-feed.md`](../agent-activity-feed.md)), so consumers immediately detect a `pmuxd` restart and reset their `sequence` cursor.

### Bounded Per-Client Buffering and Backpressure

- **Never block `ControlPlane::emit()` or other clients:** The control plane keeps its existing bounded ring (raise `DEFAULT_EVENT_CAPACITY` from `256` to `1024`, still bounded by `MAX_EVENT_CAPACITY = 4096`).
- **Per-subscriber bounded copy:** On wake, the subscriber thread locks `ControlPlane` briefly, copies at most `MAX_SUBSCRIBE_EVENT_BATCH = 128` events into a thread-local buffer (capped at 256 KiB per batch), releases the lock immediately, and writes them to the client socket with a bounded write timeout (`SO_SNDTIMEO`, e.g., 5 s).
- **Slow-consumer overflow (`lagged` frame):** If a subscriber's cursor falls behind `oldest_available_sequence()`, the server never allocates unbounded memory; it emits one structured control frame:
  ```json
  {
    "schema_version": 1,
    "boot_epoch": "e4f2c9a1...",
    "sequence": 1400,
    "ts_ms": 1764180005000,
    "kind": "lagged",
    "missed_from": 310,
    "oldest_available": 377,
    "current_sequence": 1400,
    "resnapshot_required": true
  }
  ```
  and advances the subscriber's cursor to `current_sequence` (or terminates the connection if the peer stops reading past the write timeout).
- **Bounded subscriber count:** Cap concurrent `SubscribeEvents` connections within the existing bounded client limit so runaway scripts cannot exhaust OS threads or descriptors.

---

## Structured Pane Queries

Add two read-only control requests (served off-lock for any OS process/port probing) and matching CLI commands:

### Control Verbs and CLI Surface

- `ControlRequest::InspectPane { version, request_id, pane_id, include_ports: bool }`
  - CLI: `pmux pane inspect <PANE_ID> --json [--ports]`
- `ControlRequest::ListPanes { version, request_id, session_id: Option<u64>, include_ports: bool }`
  - CLI: `pmux pane list [--session <NAME|ID>] --json [--ports]`
- `pmux events --json [--after SEQ] [--kind K]... [--session S] [--pane P] [--ports] [--timeout MS]`

### `InspectPane` Response Shape (`schema_version: 1`)

```json
{
  "schema_version": 1,
  "pane_id": 7,
  "session_id": 2,
  "session_name": "work",
  "window_id": 4,
  "window_title": "api",
  "title": "cargo test",
  "title_pinned": false,
  "status": null,
  "child_alive": true,
  "spawn_pid": 48210,
  "foreground_pid": 48299,
  "process": "node",
  "cwd": "/workspace/app",
  "last_exit": null,
  "controller_id": 3,
  "ports": [
    { "proto": "tcp", "addr": "127.0.0.1", "port": 3000, "pid": 48299, "process": "node" }
  ]
}
```

Notes on fields:
- `process`, `foreground_pid`, and `cwd` are computed **off the plane lock** using `procinfo.rs` (`PaneProcesses::read` / `cwd_of` / `live_foreground_pid` / `live_foreground_command`) from the `spawn_pid` copied briefly under the lock.
- `process` reports the executable basename only (never raw `argv`, avoiding accidental credential leakage in command-line flags).
- `last_exit` is `null` while `child_alive` is `true`, or `{ "pid": u32 | null, "exit_code": u32 | null, "signal": string | null, "exited_at_ms": u64 }` after exit.
- `ports` is omitted (`null`/absent) unless `include_ports: true` is requested; when requested, returns `[]` if none are listening or `ports_supported: false` / structured capability flag if unsupported on a platform.
- **No terminal text, scrollback, or screen cells ever appear in `SubscribeEvents` or `InspectPane`.**

---

## Listening Port Discovery (Local and Remote)

### Cross-Platform Local Discovery (`procinfo.rs`)

Given a pane's `spawn_pid`, collect its descendant PID set (bounded to e.g. 256 descendants, reusing `tree_pids` / Windows process snapshot in `procinfo.rs`), then enumerate **listening** TCP sockets (`LISTEN` state only; optionally bound UDP sockets with non-zero local port) owned by those PIDs:

| Platform | Mechanism | Permissions / Privilege | Cost & Limits |
| --- | --- | --- | --- |
| **Linux** | Walk `/proc/<pid>/fd` for `socket:[inode]` for PIDs in the pane tree, intersect with listening entries in `/proc/net/tcp` and `/proc/net/tcp6` (state `0A` = `TCP_LISTEN`). Extends existing `pid_holds_socket` pattern in `procinfo.rs`. | Same-UID unprivileged read of own `/proc/<pid>/fd`. No root required. | Fast for small trees; read `/proc/net/tcp{,6}` once per query and filter to pane-tree inodes. |
| **macOS** | `libproc` (`proc_pidinfo(pid, PROC_PIDLISTFDS, ...)` + `proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, ...)` filtering `PROX_FDTYPE_SOCKET`, `AF_INET`/`AF_INET6`, `SOCK_STREAM`, `TSI_S_LISTEN`). `procinfo.rs` already links/uses `libproc` FFI on macOS. | Same-UID unprivileged inspection of child processes owned by the current user. No root or `lsof` subprocess required. | Avoids spawning `lsof`; strictly bounded per-PID FD scan (cap e.g. 4,096 FDs/PID). |
| **Windows** | `GetExtendedTcpTable` (`TCP_TABLE_OWNER_PID_LISTENER`, IPv4 + IPv6 via `iphlpapi.dll`), matching `dwOwningPid` against the pane's ConPTY process tree from the existing Windows snapshot in `procinfo.rs`. | Standard unprivileged user API; no elevation needed. | Single OS call for all listeners, filtered in-memory against pane PIDs. |

All three platforms:
- Run **off the `ControlPlane` lock** on the connection thread (or a dedicated low-frequency port-watch thread when `PanePortsChanged` has subscribers).
- Return only `{ proto, addr, port, pid, process }` (never remote peer addresses, never packet data).
- Cap results at 64 ports per pane and enforce a per-query time budget (e.g., 250 ms) so a pathological process tree cannot stall the daemon.
- Never shell out to `lsof`, `ss`, or `netstat`.

### Remote Sessions (SSH)

Per [`docs/design/remote-spaces-ssh.md`](remote-spaces-ssh.md), remote daemons own their own sessions, processes, and control sockets; the local host does **not** forward the remote `pmuxd` socket into the local host by default.

- **Structured remote queries:** Run the remote `pmux pane inspect --json [--ports]` or `pmux pane list --json [--ports]` over a bounded, non-PTY SSH exec channel (`BatchMode=yes`, fixed command, typed IDs passed as arguments, never shell concatenation, strict output size cap and timeout)—exact same pattern as `pmux space catalog`.
- **Remote event streams:** Run `pmux events --json` over a dedicated non-PTY SSH channel with explicit cancellation on disconnect, independent `boot_epoch`, and namespaced destination identity. Never merge remote sequences into the local `ControlPlane` sequence space.

---

## Trust Model, Permissions, and No-Pane-Text Rule

1. **Control-socket trust boundary (same-user local socket):**
   - Inherits the existing [`docs/control-plane.md`](../control-plane.md) trust model: Unix socket mode `0600` (plus `SO_PEERCRED` effective-UID check on Linux), Windows user-only inheritable parent DACL.
   - Any local same-user client on the control socket can already call `Snapshot` and read session names, pane titles, `spawn` specs, and `child_pid`. `SubscribeEvents` and `InspectPane` stay within that exact metadata boundary and document explicitly that titles, `cwd`, process basenames, exit codes, and listening ports are visible to same-user local socket clients.
2. **Strict no-pane-text rule without an explicit grant:**
   - Neither `SubscribeEvents` nor `InspectPane` may ever include terminal cell text, scrollback lines, PTY output bytes, raw `argv`, or environment variables.
   - Screen/scrollback reading remains isolated to `ReadPane` / `ReadPaneStyled` / `SubscribePane`.
   - For **contained plugins and MCP exports** (`crates/prismattyc-mux/src/plugins/manager.rs`, `crates/pmux-mcp`), expose metadata-only event/query operations behind an explicit opt-in permission scope (e.g., `mux.events:local` / `mux.inspect:local`, separate from `activity.read:local` and never granting `ReadPane` text access unless a dedicated pane-read grant is explicitly approved by the operator).
3. **Data minimization:**
   - Report executable basename (`process`) rather than full `argv` to avoid leaking secrets passed on command lines.
   - Redact/bound any user-controlled strings (`title`, `status`) using existing control-plane length and control-character validators.

---

## Code Structure and Risks

| Risk | Mitigation |
| --- | --- |
| **`control.rs` monolith (~18.5k LOC)** — adding `SubscribeEvents`, `InspectPane`, and `EventWatch` inline would increase merge conflicts across parallel server work | Extract new event-streaming and pane-inspection handlers into focused submodules (`crates/prismattyc-mux/src/control/events.rs`, `crates/prismattyc-mux/src/control/inspect.rs`) and port discovery into `crates/prismattyc-mux/src/procinfo/ports.rs`. |
| **Breaking older attach/host/MCP clients on new `Event` variants** | Filter post-v1 variants out of legacy `ControlRequest::Events` responses while keeping `sequence` monotonic; add round-trip compatibility tests proving legacy v1 deserialization still succeeds when `ChildExited` and `ClientConnected` occur. |
| **Slow subscriber stalling `pmuxd` or exhausting memory** | Serve `SubscribeEvents` on the client thread off the plane lock, copy bounded batches (≤128 events / 256 KiB), enforce socket write deadlines, and emit a `lagged` frame when evicted from the bounded ring. |
| **EOF-before-reap missing exit code on Unix** | Record `last_exit` on `LivePane` with one non-blocking follow-up `try_wait()` pass before emitting `ChildExited`, falling back cleanly to `exit_code: null` if still unreapable. |
| **Port polling CPU overhead or hangs** | Keep port discovery strictly on-demand (`--ports` / `include_ports: true`) in PR 2; only run background port diffing when ≥1 subscriber asks for `include_ports: true`, always off the plane lock, with FD/PID/time caps. |
| **Credential leakage via `argv` or terminal text** | Never include PTY output, scrollback, `argv`, or `env` in events or inspect payloads; expose only executable basename, PID, `cwd`, exit status, and listening sockets. |

---

## Verification Plan for the Build PRs

- **Unit and protocol tests:**
  - `SubscribeEvents` live delivery, cursor replay, filter matching, `boot_epoch` reset, `lagged` overflow on slow consumer (proving bounded memory and non-blocking `emit()`), and clean peer-disconnect teardown.
  - Backwards-compatibility test: legacy `ControlRequest::Events` caller receives valid v1 JSON while `ChildExited` and `ClientConnected` are emitted concurrently.
  - `ChildExited` captures zero, non-zero, and signal exits for both ordinary and seat-addressed panes, and persists `last_exit` for `InspectPane`.
  - No-pane-text invariant test: writing distinctive text/secrets to a pane PTY never appears in `SubscribeEvents` or `InspectPane` JSON output.
- **Cross-platform proofs (PR 2):**
  - Bind a loopback TCP listener inside a pane child on Linux, macOS, and Windows; verify `pmux pane inspect --json --ports` reports the exact port and drops it once the listener exits.
  - Remote SSH proof using the existing isolated `scripts/remote-ssh-tests.sh` harness for `pmux pane inspect --json --ports`.
