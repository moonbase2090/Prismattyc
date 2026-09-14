# ADR-0011 — Long-lived local mux server and attach lifecycle

**Status:** Accepted (amended for interactive TTY attach; amended for the
windowed host paint contract, PT-111)
**Date:** 2026-08-11
**Depends on:** ADR-0007…0009

## Context

Binding each PTY and emulator directly to one `prismattyc-host` process is a
valid composition proof, but closing the GUI also drops the PTYs and cannot
satisfy detach. The mux needs one local process whose lifetime is independent
from every attached view.

The control plane already provides a same-user 0600 Unix socket,
connection-bound client IDs, pane controller leases, topology snapshots,
ordered events, and explicit resnapshot errors. Replacing that protocol would
discard useful security and recovery work.

## Decision

`pmuxd` is the long-lived local server process. It owns:

- the authoritative `Domain`, window bounds, layout, and event sequence;
- exactly one `PtySession` and `Emulator` per live pane;
- bounded PTY input/output queues and the maintenance loop that drains output;
- the existing versioned same-user control socket and controller leases.

`pmux-attach` is a deliberately thin reference client. A client registers
on one socket connection, takes a fresh snapshot, consumes events, and may read
a bounded visible-grid projection. A controller-authorized `WritePane` queues
UTF-8 input to the server-owned PTY; a full queue returns structured
`backpressure`. Observers remain read-only. Attach diagnoses leftover
same-uid sockets as stale instead of a generic connect failure. `--create-session`
/ `--session` exercise the control-plane session verbs.

When stdin is a TTY (and `--json` is not set), the same binary is an
interactive client. It enters raw mode, paints the pane, forwards stdin
bytes through `WritePane` after acquiring the controller lease (a
`LeaseHeld` second client stays an observer), resizes the window to the
local winsize, and detaches on `C-\` then `d` without killing the pane
child. A non-TTY stdin or `--json` keeps the original JSON dump so
detach proofs stay scriptable.

Interactive paint prefers `ReadPaneStyled` (RLE SGR runs) and
falls back to text-only `ReadPane`. The client hides the cursor for each
paint and rewrites only dirty lines after the first frame (full repaint
on first paint, resize, or row-count change). DEC 2026 is skipped until
the outer host implements it.

`ReadPaneStyled` accepts optional `view_offset` (rows back from the
live tail). The server clamps to `max_view_scroll` and reports both fields.
Absent offset is the live tail. The interactive client enters scroll mode
on PageUp or `C-\ [`; arrows/PgUp/PgDn/Home/End move; Esc/q returns to the
tail. Keys are not forwarded while scrolled. New output keeps the view
anchored. Older servers omit the fields and the client hides scroll
mode.

Attach paint disables DECAWM (`CSI ? 7 l`) for the outer terminal
so a full-width row does not wrap. Wrap is restored on detach.
Erase-line is emitted *before* the glyphs. After a full-width write
the cursor sits on the last column, so a trailing EL would erase it
(plain and styled). The client does not clip to width-1.

### Lifetime and identity

- `ClientId` is minted by the server and bound to exactly one live connection.
- An idle socket read timeout wakes the server loop but does not detach the
  client or revoke its leases.
- Connection loss clears that client's leases. It does **not** remove panes,
  kill children, or discard emulator state.
- Server termination/crash is not detach. This decision does not provide cold
  resurrection or restart persistence.

### Live topology transactions

Split spawns the new pane runtime and applies authoritative geometry before its
event is committed. Spawn/resize failure rolls the topology back without an
event. Resize applies to every affected PTY and emulator atomically. Close
resizes survivors before dropping the closed pane runtime.

### View projection

`ReadPane` returns revision, geometry, cursor/alt/child status, and visible text
rows. The full screen and scrollback remain server-owned. This bounded
projection proves the attach boundary; styled GUI synchronization can evolve
without moving emulator ownership back into the GUI.

### Amendment (PT-111) — paint contract for the windowed host

`prismattyc-host` no longer paints an attached session from `ReadPaneStyled`
through a nested `pmux attach` PTY child. Its paint contract is **log +
snapshot**:

- The host opens two control connections per attached pane. One parks in
  `SubscribePane { pane_id, from_seq }` (PT-77) and feeds `Output` and
  `Resize` events into that pane's own `Emulator`. The other sends key bytes
  with `WritePane` under the controller lease and host geometry with `Resize`.
- `from_seq` older than the ring returns a `PaneStyled` snapshot plus
  `through_seq`. The host repaints from the snapshot with cursor addressing
  and tails from `through_seq`.
- The replica is a read replica. It never forwards `take_pending_replies()`:
  DSR/CPR/DA answers belong to the PTY owner
  ([pane-event-log-spike.md](../pane-event-log-spike.md) §2).
- The server is still the single writer and the single PTY owner. Nothing
  about pane ownership, leases, or lifetime moves.

Consequences: one emulator per attached pane instead of two, the host
scrollbar and find work over the session's real scrollback, and the PT-53
escape-passthrough trap is gone.

Known limit in this pass: the replica has no emulator state import, so when
the whole log is still in the ring it replays from seq 1 at the host pane's
current size. History written at an older size can wrap differently in
replica scrollback until an emulator snapshot API lands. The visible screen
converges on the next `Resize` or output. The snapshot path has no such
limit.

`pmux-attach` is unchanged outside the host: it stays the thin
`ReadPaneStyled` reference client. Under `PRISMATTYC_HOST=1` it writes
the attach-tabs cache so the host opens a log replica (PT-306).
`PRISMATTYC_ATTACH_PTY=1` restores the nested-child host path. A nested
attach that remains under the host paints the host scroll chip and
right-edge bar.

## Consequences

- A GUI/control client may disconnect while PTYs continue under the server.
- The same protocol supports attach clients, automation, and Termwright helpers
  without screen scraping for topology.
- The server remains local-only. There is no remote multi-machine transport in
  this decision.

## Rejected alternatives

- GUI-owned PTYs plus a metadata daemon: closing the GUI still kills children.
- A second unversioned attach socket: duplicates identity, leases, and resync.
- PTY byte-stream fan-out to client emulators: creates multiple VT brains that
  can diverge after reconnect or dropped bytes.
- Daemonizing/forking inside Prismattyc: supervision policy belongs to the launcher;
  the server binary itself has one explicit process lifetime.
