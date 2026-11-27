# Parking idle panes

Status: **proposal from a time-boxed spike, not accepted direction.** Nothing
here changes shipped behavior. It depends on the two-part pane snapshot format
in `docs/design/pane-snapshot-format.md`, which is being written alongside this
note.

Base: `origin/main` `05458628`. Line references are to that commit.

## Summary

Parking frees most of an idle pane's memory while its child process, PTY and
reader keep running. When a pane has been quiet for a while and no client is
watching it, the pmux server writes its emulator state to disk in the two-part
snapshot format, encrypted under a key that lives only for this server run.
The server then drops the emulator and keeps only the small screen part in
memory. The first PTY output, a resize, or a read that needs history brings the
pane back. A client that attaches to a parked pane gets its first frame from the
in-memory screen part, without unparking.

**Recommendation: go**, once the snapshot format has merged. The build is about
23 agent-hours over 4 PRs, about 6 working days with review and the Windows
proof (see [Build plan](#build-plan)).

## What parking saves

These are per-pane costs today. They come from `docs/design/grid-history-profile.md`
and the constants in the code.

| Held in memory | Size |
|---|---|
| Scrollback, 10,000 rows (`MAX_SCROLLBACK`, `live.rs:37`) | 29 MB at 80 columns (profiled); about 43 MB at 120 and 72 MB at 200 (36-byte `Cell`, `history_budget.rs:9`). Capped by the 96 MiB byte budget. |
| Pane log ring (`DEFAULT_PANE_LOG_BYTES`, `pane_log.rs:15`) | up to 4 MiB |
| Live grids, styles, hyperlinks, graphics | small for text panes; graphics are stored as raw RGBA and can be large |

A parked pane keeps its screen part (the visible grid, cursor, modes, and the
styles those rows use), on the order of rows × columns × 36 bytes, and its PTY
and thread overhead. For a fleet of agent panes with full history, parking cuts
resident memory by roughly the scrollback size per idle pane. The build
measures this (see [Proof](#proof-for-the-build)).

Parking does not save threads. Each pane keeps its reader and writer threads
(`LivePane::spawn`, `live.rs:261`). Moving idle readers onto a shared reactor is
a separate piece of work, and parked panes are its natural first candidates.

## How a pane runs today

- The reader thread reads 8 KiB chunks from the PTY and sends them on a channel
  bounded at 64 chunks (`FROM_PTY_CAP`, `live.rs:34`). An 8 MiB `ByteBudget`
  blocks the reader when queued output is not drained (`pane_log.rs:19`).
- The `prism-server-runtime` thread takes the plane lock every 4 ms and drains
  each pane (`ControlPlane::drain_live`, `control.rs:3022` →
  `LivePane::drain`, `live.rs:628` → `apply_pty_chunk`, `live.rs:580`). That
  is the only place bytes reach the emulator, so it is the single writer.
- Output is parsed whether or not a client is attached.
- `check_pane_idle` (`control.rs:2946`) fires `PaneIdle` after 60 s with no
  output (`PANE_IDLE_TRIGGER_MS`, `control.rs:369`).
- There is no per-pane count of watching clients. The nearest state is
  `client_views`, `client_pane_focus` and `active_clients` on the plane.
- `Emulator::export_state` (`prismattyc-emulator/src/lib.rs:494`) clones the
  grids and the whole scrollback, and refuses while the parser is inside an
  escape sequence or a graphics upload.
- Pane-log persistence for restart restore runs on the `pmux-checkpoint` worker
  (`pane_log_persist.rs:42`). It writes a plaintext JSON snapshot plus a journal,
  owner-only (`private_fs.rs`), at most every 2 s. At shutdown,
  `flush_pane_logs` (`control.rs:3267`) writes every pane synchronously under
  the plane lock.

## Design

### When a pane parks

All of these must hold:

1. No output for the park delay. Default 5 minutes. Configurable, and `0`
   turns parking off. The 60 s idle trigger is too short for agent panes that
   pause to think.
2. No client is watching the pane: it is not in any client's view or focus,
   and it has no open `SubscribePane` stream. The build adds a per-pane watcher
   count, raised and lowered by `handle_subscribe_pane` (`control.rs:8072`).
3. The parser is at a boundary and no graphics upload is pending (export would
   fail otherwise; retry on the next tick).
4. The child is running. A pane whose child exited, or a Windows pane in the
   exit-draining state (`ChildExitState::Draining`, `live.rs:149`), does not park.
5. The history is worth parking: at least 1 MB of scrollback. Small panes stay
   resident, because the file and key overhead outweighs the saving.
6. The parked-file budget has room (see [Encryption and files](#encryption-and-files)).

### Park

1. **Checkpoint first.** Force a full pane-log persist capture for the pane
   through the existing `capture_persist` path. Restore after exit then never
   needs the parked file (see [Restore after exit](#restore-after-exit)). When
   pane-log persistence is off, this step is skipped.
   Today that capture is a full export under the plane lock, about 22 ms for a
   10,000-row pane, once per park. It can move to history paging later, along
   with the regular checkpoint.
2. **Cut the screen part** under one short plane-lock hold, as the snapshot
   format's cut procedure describes. The companion note measures this at about
   0.03 ms for a 10,000-row pane, against 22 ms for today's full export.
3. **Page the history out.** Each page takes the lock briefly and exports at
   most 256 rows by absolute index (about 0.6 ms per page). The park worker
   encodes, encrypts and appends each page off the lock, then writes the trailer
   and fsyncs. Output during the park cancels it (see [Unpark](#unpark)).
4. **Drop the history.** Under one more short hold, check that the history
   generation and `through_seq` are unchanged since the cut. Then replace the
   pane's emulator with a screen-only emulator, imported from the screen part
   with empty history. If anything moved, abandon the park and delete the file.
   A failed park costs nothing but the attempt.

A parked pane therefore still holds a real `Emulator` in `LivePane.emulator`
(`live.rs:164`), just one with no history. Every path that reads the emulator
today (`content`, `styled`, `keyboard_flags`, `pane_state_snapshot`,
`ReadPane`, `ReadPaneStyled`, guest keyboard flags) keeps working unchanged. The
pane carries a small `parked: Option<ParkedFile>` beside it with the file, the
generation and the absolute history range.

### Unpark

Triggers:

- **PTY output.** In `LivePane::drain`, a parked pane stops draining the moment
  a chunk is waiting. It does not apply it. The chunk and any later ones stay in
  the bounded channel in order. Once the channel or the byte budget fills, the
  reader blocks and the kernel holds back the child. No byte is dropped or
  reordered, and the runtime thread remains the only writer.
- **Resize.** The grid and history must reflow, so a resize unparks first and
  is applied after.
- **A history read:** `ReadPaneState` past the screen part, or a scrollback
  request from a client.

Input to a parked pane goes straight to the child, as today. The echo is
output, so it unparks the pane.

The park worker reads, decrypts and decodes the history chunks off the lock. On
the next tick the runtime prepends them to the screen-only emulator, using the
core screen's new prepend API (the same one the host uses to fill history behind
a screen-first attach), then drains the queued chunks normally. Nothing has
changed the screen or history since the park, because no output was applied, so
the generation still matches. If the file is missing or does not decrypt or
decode, the pane unparks with a gap in its history and logs one warning, so
output is never held back for long.

If output arrives while a park is still paging out, the park is abandoned: the
pane still holds its full emulator, so it just drains normally and the partial
file is deleted.

### Attach to a parked pane

The first frame comes from the in-memory screen part, as the staged attach in
the snapshot format describes. The pane is not unparked. History parts are read
from the file, decrypted one part at a time by the worker, and streamed after
the first frame. The pane log ring is cleared at park, so a `SubscribePane`
catch-up from before the park gets a gap and the styled screen frame, as it does
today when a client falls behind (`subscribe_frame`, `control.rs:6892`).

### Encryption and files

- **Format and envelope are separate layers.** The parked file holds the
  plaintext two-part format, with each part sealed in an envelope that carries
  a key id, a nonce and an authentication tag. The snapshot format itself has no
  key fields, so other users of it can seal it under their own keys.
- **Key:** 32 random bytes from `getrandom` at server start. It is held only in
  memory, never written, and zeroized when the server stops (`zeroize` is
  already in the lockfile through `rustls`). The cipher is ChaCha20-Poly1305
  (the `chacha20poly1305` crate). The authenticated data is the pane id plus
  the part header, so parts cannot be swapped between files or panes.
- **Nonces:** the 96-bit nonce is a 64-bit file id followed by the 32-bit part
  index. File ids come from one per-run counter (an `AtomicU64` on the server,
  starting at 1). Every park attempt takes the next id, including attempts that
  are abandoned and parks of a pane that parked before, so an id is never used
  twice under a key. Parts are written once and never rewritten in place. A
  file that has to change is written again under a new id. A file holds at most
  2^32 − 1 parts, far above the 256 MiB per-pane cap. The counter cannot wrap
  in practice; if it would, the server stops parking until it restarts. The key
  changes every run, so ids restart safely at 1. Each (key, nonce) pair is used
  for exactly one part.
- **What this protects, and what it does not:**
  - Parking does not add a readable copy of history to disk. Without the
    envelope, every parked pane would leave a second plaintext copy of its
    history in `parked/`, including after a crash.
  - It does not make history at rest safer than today. With pane-log
    persistence on (the default), the plaintext checkpoint written before the
    pane parks holds the same history, as it does today for every pane, parked
    or not. A copy of the data directory or a backup exposes that history
    exactly as it does now.
  - With persistence off (`PMUX_PANE_LOG=off`), there is no checkpoint, and the
    encrypted parked file is the only copy of the history on disk.
  - It does not protect against a local user who can read the server's memory,
    who can read the live panes anyway.
  - Protecting the checkpoint itself is part of the restart-persistence
    decision noted in [Restore after exit](#restore-after-exit).
- **Files:** one file per parked pane in a `parked/` directory under the
  server's per-instance data directory. The directory is 0700 and the files
  are 0600, created through `private_fs.rs` like the pane log. On macOS the
  directory is marked to be left out of Time Machine backups. Files are deleted
  on unpark and on clean exit. At startup, a leftover `parked/` directory from
  a crashed run is deleted, since its key is gone.
- **Size cap:** each file is bounded by the pane's scrollback byte budget. The
  whole directory has a cap (default 2 GiB, configurable). When the cap is
  reached, new panes stop parking. Nothing already parked is evicted.

### Restore after exit

Parked files never feed restore. Because a pane writes a full pane-log
checkpoint just before it parks, and any output unparks it, that checkpoint
stays exact for as long as the pane is parked. At shutdown, `flush_pane_logs`
skips parked panes, because their checkpoint is already current. Restore after
an update or restart works exactly as it does today, and v1 needs no long-lived
key.

The checkpoint is plaintext, as it is today, so parking leaves history at rest
exactly as exposed as it is now: no less, and no more. Encrypting restart
persistence needs a key that outlives the server run, and is its own decision,
not changed here. If that is decided later, it covers parked and resident panes
alike, and the parked files need no change.

With pane-log persistence off, parking skips the checkpoint, and a parked pane
is not restored after exit, the same as any other pane with persistence off.

A side effect: the shutdown flush is synchronous under the plane lock today, so
skipping parked panes makes shutdown faster in a large session set.

### Lock rules

- Under the plane lock: the screen cut, each 256-row history page, the final
  check-and-swap to a screen-only emulator, and the prepend on unpark. Each is a
  short, bounded hold. None encodes, encrypts or does I/O.
- Never under the plane lock: encoding, encryption, file I/O, decoding, or
  streaming history to a client.
- The park worker never takes the plane lock while it holds a file open.

### Windows

Parking sits above `portable-pty`. The emulator is ours, not the console host's,
so the design is the same on Windows:

- The ConPTY reader thread and pseudoconsole stay open while a pane is parked.
- ConPTY may repaint on resize. That arrives as output, which unparks the pane,
  as it should.
- Panes in `ChildExitState::Draining` never park.
- The close path (`LivePane` Drop, `live.rs:196`) answers the console host's
  cursor request on a separate thread. The build must confirm it does not read
  the emulator. If it does, it reads the cursor from the screen part.
- File permissions: `private_fs.rs` sets owner-only modes on Unix. The build
  must give the `parked/` directory an owner-only ACL on Windows, or confirm
  that the per-user data directory already does.

The build carries a clean Windows VM proof (see below).

### Out of scope

- Handing a live PTY to a new server process. Parking keeps the PTY in the same
  process.
- Program resume across restarts.
- Moving idle readers off per-pane threads.
- Encrypting restart persistence.

## Open questions

1. Is 5 minutes the right default park delay, and should agent panes or
   foreground shells get different ones?
2. Should a pane that is visible in a client's layout but not focused count as
   watched? This note says yes, to avoid unparking on every glance.
3. Is a 2 GiB default directory cap right for laptops?

## Build plan

| PR | Content | Agent-hours |
|---|---|---|
| 1 | The park trigger, the watcher count, the park worker, paging history out, the check-and-swap to a screen-only emulator, and unpark on output, resize and history reads (prepend) | 8 |
| 2 | Parked files: envelope encryption, the run key, caps, cleanup, backup exclusion, failure fallback | 6 |
| 3 | Attach and history streaming from a parked file, checkpoint-before-park, and skipping parked panes in the shutdown flush | 5 |
| 4 | Windows: close path, ACLs, clean VM proof; memory measurement written into `docs/design/` | 4 |

That is about 23 agent-hours and about 6 working days, at roughly one merged PR
a day once review and CI are counted, plus the Windows proof. All four PRs
build on the snapshot format work: the parts encoder, absolute history paging
and the prepend API. Those land first and are sized in the companion note.

## Proof for the build

- A round trip (park, unpark, export) equals the export taken before parking.
- No byte loss: a pane under a 10 MB output burst, parked and unparked
  repeatedly, ends with the same screen and history as a control pane that never
  parks.
- Attaching to a parked pane paints the first frame without unparking, and the
  history matches once streamed.
- A killed server leaves no readable history. The leftover files fail to
  decrypt, and the next start deletes them.
- Memory: resident size for 200 idle panes with full scrollback, before and
  after parking, recorded in the measurement doc.
- Windows: the same tests on a clean Windows VM, including closing a parked pane.
