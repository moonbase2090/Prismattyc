# Pane event log spike (PT-72)

**Status:** Research spike. No protocol change lands with it. One regression
test lands: `crates/prismattyc-emulator/tests/replay_determinism.rs`.
**Date:** 2026-08-28
**Origin:** owner idea — "replicated state machines for the terminal sessions".
**Lane:** docs + one test. Cites current code with `file:line`.

## Recommendation

Replicate the **emulator**, not the process. A pane is a deterministic state
machine over an ordered log of `Output(bytes)` and `Resize(cols, rows)`
events; the experiment below proves the emulator we ship already has that
property. Ship it in three steps, each additive under ADR-0008 v1:

1. **Log + subscribe in pmuxd.** Keep a bounded per-pane event log with a
   real sequence number; add `SubscribePane { from_seq }` that streams
   `PaneEvent`s. Keep `ReadPane*` as the snapshot verb.
2. **Host consumes the log.** prismattyc-host attaches a pmuxd pane as a
   log subscriber feeding its own `Emulator`, replacing the nested
   `pmux attach` PTY child. One emulator per pane instead of two.
3. **Snapshot + tail.** Add an emulator state export/import so a subscriber
   can start from a snapshot at seq *N* and replay the tail; then persist
   snapshot + tail for screen durability across a pmuxd restart.

Do not replicate across machines in this pass. The PTY child cannot move, so
multi-node is log shipping for read replicas, and it needs a transport ADR-0008
declares local-only.

## 1. How pane content reaches a viewer today

```
child ─PTY─▶ reader thread ─mpsc(64)─▶ LivePane::drain ─▶ Emulator::feed
                                                           │  revision += 1 per chunk
                                                           ▼
   pmux-attach (TUI) ── every 50 ms ── ReadPaneStyled ──▶ full visible grid + RLE runs
        │ paints ANSI into its own PTY
        ▼
   prismattyc-host PaneRuntime ── its own Emulator re-parses that ANSI ── raster
```

| Fact | Where |
|---|---|
| Read verbs are request/response only: `ReadPane`, `ReadPaneStyled { view_offset }`, `Snapshot` (topology, no text), `Events { after_sequence }`. No subscribe verb for content. | `control.rs:588`, `:600`, `:315`, `:319` |
| `PaneContent` carries the visible grid only (`lines`, cursor, `revision`); `PaneStyled` adds RLE style runs, `view_offset`, `max_view_scroll`. No scrollback in either. | `control.rs:1361`, `:1530` |
| `rev` = `LivePane::revision`, bumped once per PTY read chunk, per guest resize, per child death. It is a dirty bit, not a log index. | `live.rs:131`, `:415`, `:308`, `:259` |
| The event ring has **no** `PaneOutput` and **no** pane `Resize` event. `OutputActivity { pane_id, revision }` is coalesced to one entry per pane and carries no bytes. Ring capacity 256 (max 4096); overrun ⇒ `EventGap` + resnapshot. | `control.rs:1640`, `:4815-4852`, `:33-35`, `:4713-4728` |
| Raw bytes are fed and dropped; no ring, no log. Only byte *counts* are recorded, for mail-inject quiet detection. Scrollback cap 10 000 lines. | `live.rs:419`, `control.rs:2008`, `live.rs:24` |
| `pmux-attach` polls stdin at 50 ms and calls `ReadPaneStyled` every tick; repaints when `revision` or `view_offset` changed. The whole grid crosses the socket on every change. | `pmux-attach.rs:43-46`, `:2209`, `:2393`, `:2414` |
| The windowed host never talks to pmuxd for content: it spawns `pmux attach --session-id ID` as a PTY child and runs a second `Emulator` over it. Zero hits for `ReadPane` in `prismattyc-host`. | `main.rs:341`, `:369-393`, `mux.rs:236`, `:325` |
| ADR-0011 names `ReadPaneStyled` (with `ReadPane` fallback) as the paint contract and calls pmux-attach "a deliberately thin reference client". That is the decision this spike would amend. | `docs/adr/0011-long-lived-mux-server.md` |

Consequences of the current shape:

- **Double emulation** in the host. Escapes an agent prints are consumed by
  the server emulator and re-emitted by pmux-attach (the PT-53 trap);
  OSC/APC passthrough, search over real scrollback, and zoom of a nested
  pane all fight this.
- **Bandwidth is per frame, not per change.** One 80×24 styled frame is
  1 920 cells of style runs; the recorded session below is 12 400 bytes of
  *log* for 339 lines of history.
- **No catch-up and no history transfer.** A reconnecting viewer gets the
  visible grid; scrollback beyond it is reachable only page by page via
  `view_offset`.
- **Nothing survives a pmuxd restart** except the mailbox (SQLite,
  `mailbox/store.rs:96`). Pane screens are lost with the process.

## 2. Experiment: is the emulator replay-deterministic?

Harness: record one real PTY session's bytes, then replay them into
independent `Emulator`s under different chunkings and compare `Screen`
(`Screen: PartialEq + Eq`, `prismattyc-core/src/lib.rs:459`).

Session script: `ls --color`, SGR bold/underline/256-color, `seq`, wide CJK
and emoji, `\e[2J\e[H`, `top -bn1`, the alternate screen, a scroll region.
12 400 bytes recorded at 80×24. Bigger run: 60 000 colored lines, 2.7 MB.

| # | Scenario | Expect | Result |
|---|---|---|---|
| S1 | whole feed vs byte-by-byte feed | equal | **PASS** |
| S2 | whole vs random chunks (seeded, ≤ 97 B) | equal | **PASS** |
| S3 | whole vs 3-byte chunks (every escape and UTF-8 sequence split) | equal | **PASS** |
| S4 | resizes 80×24→100×30→60×20 at fixed byte offsets: chunked vs bytewise | equal | **PASS** |
| S5 | same resizes: chunked vs random chunks | equal | **PASS** |
| S6 | control: same resizes at offsets shifted by +1 500 B | differ | **PASS** (differ) |
| S7 | control: no resize vs resized | differ | **PASS** (`history_len` 339 vs 337) |
| S8 | 2.7 MB log: 4 KiB chunks vs 777 B chunks | equal | **PASS** |

Timing (debug harness, release build): 12 400 B replays in ~1–2 ms; 2.7 MB in
170 ms ≈ 16 MB/s, scrollback capped at 10 000 lines.

What the experiment establishes:

1. `Emulator::feed` is chunk-independent, including cuts inside CSI/OSC
   sequences and multi-byte UTF-8. A log can be shipped in any framing.
2. Resize is part of the state and must be logged **at its byte position**.
   The core's resize is a clipping copy with no reflow
   (`prismattyc-core/src/lib.rs:2383`, `:2452`), so it is lossy and
   non-invertible: replaying output against a different final size does not
   reproduce the live screen (S6, S7). This is a constraint, not a defect.
3. No time, randomness, or environment enters `feed`/`resize`/`screen`
   (the only `SystemTime` uses are a terminfo temp-file name and tests).
4. Two things a replica must **not** do: re-send `take_pending_replies()`
   (DSR/CPR/DA answers belong to the PTY owner only), and construct the
   emulator differently from the recorder (`new` vs `new_experimental`
   changes APC collection).

The harness is preserved as `tests/replay_determinism.rs` with the 12 KB
session as a fixture, so the property is guarded from now on.

## 3. Proposed model

```
PaneLog (per pane, in pmuxd)
  seq: u64              monotonic, +1 per event, never reused
  events: ring of PaneEvent
  snapshot: { seq, cols, rows, emulator state }   taken at compaction

PaneEvent
  Output   { bytes }                       from the PTY reader
  Resize   { cols, rows, cell_px }         from Resize / reconcile_workspace_geometry
  Title    { text }                        OSC 0/2 already parsed
  Cwd      { path }                        OSC 7
  Status   { text | clear }                pmux status-set (PT-51)
  Attention{ text }                        PT-53
  MailDepth{ depth }                       doorbell state
  Exited   { code | signal }               observe_death

SubscribePane { pane_id, from_seq }  →  stream of { seq, event }
  from_seq older than the ring  →  { snapshot, then events }   (like EventGap today)
```

Rules:

- pmuxd remains the single writer (the PTY owner). Input keeps the
  controller lease (PT-52); the log is output-side only.
- `Output` events are appended per read chunk exactly where `feed` is
  called today (`live.rs:419`); `Resize` where `emulator.resize` is called
  (`live.rs:297`). Order in the log equals order applied to the server
  emulator, so server and replicas are the same machine.
- `ReadPane*` stays for thin clients and old binaries; it becomes a read of
  the server replica. `OutputActivity` stays as the cheap hint.
- Ring size in bytes, not events (e.g. 4 MiB per pane); compaction =
  snapshot at the ring's oldest seq.
- Emulator state export/import is new API (`Emulator` is not `Clone`; the
  vte parser state and mode stacks must be included with `Screen`).
  Version it: a snapshot records the emulator version, and a mismatch
  forces a fresh `ReadPane`.

## 4. What it buys, what it costs

| Payoff | Why it follows | Ticket it touches |
|---|---|---|
| Host un-nesting: one emulator per pane, no `pmux attach` PTY child | host subscribes and feeds its own `Emulator` | PT-53 trap gone; PT-57 zoom and find over real scrollback; PT-68 placeholder can keep the last screen |
| Reconnect / catch-up | snapshot + tail replay at 16 MB/s | SSH attach (`docs/ssh-mux-attach-spike.md`), shared attach (PT-52) viewers provably identical |
| Bandwidth | bytes per change vs grid per change | pmux-attach TTY is event-driven (`SubscribePane`); `PRISMATTYC_ATTACH_POLL=1` restores 50 ms `ReadPaneStyled` |
| Screen durability | persist snapshot + tail as `pane-log-<instance>.json` next to `mail.db`; restore matches session name + pane index; snapshot is `emulator-state-v1` | pmuxd restart keeps history beyond the tail; PTY children still respawn |
| Evidence | log export = `pipe-pane` / `save-buffer` | closed — PT-116 |
| Third viewer | web or remote client is one more subscriber | capability matrix T4/T5 get a real transport later |

| Cost | Size |
|---|---|
| Determinism becomes a contract: emulator version in snapshots; any change to `feed`/`resize` semantics must keep `replay_determinism` green | ongoing, small |
| Emulator state export/import (parser, modes, keyboard stacks, graphics) | M |
| Log ring + compaction + `SubscribePane` streaming on the NDJSON socket (today one request → one response; a stream needs a long-lived response or a second connection) | M |
| Host: a log-backed pane kind next to PTY-backed panes; input still goes through `WritePane` | M–L |
| ADR-0011 amendment (paint contract) and ADR-0008 addendum (streaming verb) | S |

### Snapshot size note

The state DTO stores row text, compact style runs, and one deduplicated style table.
The previous per-cell DTO measured 3,368,199 bytes with 128 scrollback rows and
192,199,820 bytes with 10,000 rows on the PT-72 fixture. The compact v1 DTO
measures 1,752,695 bytes for the 10,000-row case. Plain row text is about
800,000 bytes for 80 columns and 10,000 rows. Keep the configured scrollback
bound in the snapshot so readers can reject oversized input before allocation.
The serializer and compression choice belong to the persistence ticket.

What does not change: `Session → Window → Pane`, the mailbox, leases, the
host tab arrangement, PT-63 polish work.

## 5. Open questions

1. Streaming over the current one-shot NDJSON request model: long-lived
   response on the same connection, or a dedicated subscribe connection per
   pane? (`MailWait` already blocks a connection; precedent exists.)
2. Snapshot format: serde of `Screen` is straightforward; the vte `Parser`
   is opaque — snapshot only at a clean parser state (between sequences),
   which the writer can guarantee since it owns the parser.
3. Graphics (Kitty images) in the log: image payloads are large; log the
   APC bytes as `Output` (simple) or reference an image store (smaller).
4. Rich workspace rows (`reconcile_workspace_geometry`) are a resize
   input; they must be logged as part of `Resize`.

## 6. Follow-up ticket titles (owner files after approval)

- feat(mux): per-pane event log with real sequence numbers (`PaneEvent`, ring by bytes)
- feat(mux): `SubscribePane { from_seq }` streaming verb (ADR-0008 addendum)
- feat(emulator): state export/import for snapshots (versioned)
- feat(host): log-backed attach pane (subscribe + local emulator; retire nested `pmux attach`)
- feat(mux): pmux-attach consumes the log (event-driven paint, catch-up)
- feat(mux): persist snapshot + tail next to mail.db; restore screens on restart
- feat(mux): `pmux pipe-pane` / `save-buffer` as log export
- docs: ADR-0011 amendment — paint contract becomes log + snapshot

Order: log → subscribe → host pane → attach → snapshot API → persistence.
The first two are independent of the emulator work and unblock the host.
