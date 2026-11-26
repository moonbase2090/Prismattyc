# Pane snapshot format and screen-first attach

Status: design note, proposed. The parking work and restore after the server exits will reuse the format defined here.

## Problem

A client that attaches to a pane gets nothing to show until it has received the whole pane. The GPU host rebuilds its copy of a pane from `ReadPaneState`, which sends the complete emulator state. That includes up to 10,000 history rows, and the visible screen only arrives with them. The pmux server also builds that state while it holds the one lock every pane and request shares. A pane with a long history therefore delays its own first frame, and briefly stalls every other pane on the server.

The goal is to show the screen first and fill in history afterwards. The same snapshot should serve three users: attach, parked panes written to disk, and restore after the server exits. One format means one encoder, one decoder and one set of tests.

## Goals

- The first frame needs only the visible screen: grid, cursor, modes, alternate screen and images.
- History arrives afterwards in chunks addressed by absolute row number. The client can page it on demand or in the background.
- The server never holds the shared lock while history is encoded or sent.
- Each part stands alone, so a parked file can serve the screen part without reading its history.
- Anything that changes between the screen cut and a history fetch is reported explicitly as a gap or a restart, never silently.
- Old clients and old servers keep working.

## Non-goals

- Encryption. The plaintext format carries no key fields; an outer envelope is defined by the code that stores parts (see "Parking and at-rest").
- Moving a live PTY between processes.
- Changing `pmux-attach`'s text paint path, which already reads only the visible screen (below).
- Restoring the PTY or the program. That is the program-resume work.

## How it works today

### Locks

The pmux server has one control-plane mutex. Live panes (`LiveRuntime` and its `panes` map, `crates/prismattyc-mux/src/live.rs:856-857`) are owned by the control plane, so every pane read and every PTY feed runs under that mutex.

- PTY output is fed to each pane's emulator on the `prism-server-runtime` thread. The thread holds the plane lock for `drain_live` (`crates/prismattyc-mux/src/control.rs:7671-7676`, `control.rs:3022`), which reaches `emulator.feed` at `live.rs:594`. The same lock hold also runs `maybe_persist_pane_logs`.
- `ReadPaneState` is served on the client thread (`control.rs:4525`). It takes the plane lock, calls `pane_state_snapshot` → `Emulator::export_state()` (`control.rs:8224-8229`, `control.rs:6966-6993`, `live.rs:1272-1279`), releases the lock, and only then serializes to JSON and streams 128 KiB `PaneStateChunk` frames, capped at 256 MiB (`control.rs:56-57`, `control.rs:8230-8280`). The expensive part, `export_state()`, therefore runs under the lock.
- The pane-log checkpoint also calls `export_state()` under the plane lock (`live.rs:1421`) and leaves only encoding and disk I/O to the `PersistWorker` (`crates/prismattyc-mux/src/pane_log_persist.rs:33-46`). It runs up to every 2 seconds for each dirty pane (`pane_log_persist.rs:28`).

### Readers

- **`pmux-attach`** polls `ReadPaneStyled` (`crates/prismattyc-mux/src/bin/pmux-attach.rs:1260`, `:6031`). That request returns only the visible grid and style runs, plus `view_offset` to page back through history one screen at a time (`control.rs:874-887`, `live.rs:748`). Text attach is already screen-first, and this note does not change it.
- **The GPU host** rebuilds a full emulator replica from `ReadPaneState` (`crates/prismattyc-host/src/attach_log.rs:590-600`, `:673-690`, `:1493`). It then streams output from `through_seq` with `SubscribePane` (`control.rs:550-562`). It cannot paint the pane until the whole state has arrived and been imported.

### What the emulator exports

`EmulatorStateV1` (`crates/prismattyc-emulator/src/lib.rs:213-237`) holds `ScreenStateV1` plus modes (mouse, keyboard stacks, bracketed paste, cursor shape), cwd, cell pixel size and `GraphicsStateV1`. `export_state()` refuses to run in the middle of an escape sequence (`lib.rs:494-497`), which gives a parser-boundary cut for free.

`ScreenStateV1` (`crates/prismattyc-core/src/lib.rs:698-720`) has:

- one `styles` table and one `hyperlinks` table, shared by grid rows and history rows (`RowStateV1`, `lib.rs:733-744`, styles as runs and hyperlinks by handle);
- the primary and alternate grids with cursors and margins;
- `scrollback: Vec<RowStateV1>`;
- the counters `scrolled_lines`, `content_epoch` and `full_clears`;
- `retain_alt_history` and `alt_history_rows`.

### How the core screen stores history

`Screen` (`crates/prismattyc-core/src/lib.rs:605-650`) keeps history as `VecDeque<Vec<Cell>>` with a parallel wrapped-flag deque.

- Cells carry their `Style` inline; the export builds the style table.
- Grapheme tails live in a shared `ClusterStore`.
- History is capped by rows and by a 96 MiB byte budget (`lib.rs:807`).

Three facts matter for addressing history:

- `scrolled_lines` counts only scroll-ups of the primary grid (`lib.rs:2461-2466`). Rows that the alternate screen pushes into history, which the mux enables, do not count. So `scrolled_lines` is not the number of rows ever added to history.
- `CSI 3 J` replaces the history with an empty deque (`lib.rs:3103-3106`).
- A resize reflows the whole history. The rows are rebuilt, and the row count can change (`crates/prismattyc-core/src/reflow.rs:160-176`).

### Images

`GraphicsStateV1` (`crates/prismattyc-emulator/src/graphics/mod.rs:72-78`) holds placed and virtual images plus a registry of uploaded image data. Each frame is capped at 8 MiB and each generation at 16 MiB retained (`graphics/mod.rs:18-19`). Today it rides inside the single state blob.

## Measurements

The setup was a pane of 200 columns by 50 rows, with 10,000 history rows of styled log text (SGR colours, bold, dim, 256-colour, and an OSC 8 link on every line). It used a release build and 31 runs. The harness drove the public emulator API, as the pmux server does.

| Step | Median | p95 | Bytes |
|---|---|---|---|
| `export_state()`, run under the plane lock today | 22.4 ms | 24.3 ms | n/a |
| JSON encode of the full state, after the lock | 3.4 ms | 3.9 ms | 4.78 MB |
| JSON encode of history rows only | 3.2 ms | 3.6 ms | 4.53 MB |
| Copy of the visible grid (characters and styles) | 0.03 ms | 0.05 ms | 240 KB in memory |
| Deep clone of the whole `Screen`, history included | 2.4 ms | 3.8 ms | n/a |

Conclusions that do not depend on the machine:

- Building the export is what costs time, about seven times the JSON encode. The lock is held for all of it.
- History is about 95% of the bytes. The screen part is about 5% here, and less on panes with narrower history.
- Copying the visible screen is roughly 700 times cheaper than exporting everything. A screen cut is effectively free.
- Cloning the raw rows is about nine times cheaper than exporting them. This gives a simple fallback if paging under short lock holds proves awkward (see "Cut procedure").
- The pane-log checkpoint pays the same `export_state()` cost under the lock for each dirty pane. Reusing the history pages for checkpoints removes most of it. That is follow-up work, not part of this build.

## The format

A snapshot is a sequence of parts. Each part stands alone: a reader can decode one without the others, apart from the image references described below.

### Part frame

```
magic        4 bytes   "PSNP"
version      u16       format version, starting at 1
kind         u8        1 = screen, 2 = history chunk, 3 = gap, 4 = image, 5 = trailer
flags        u8        reserved, zero
length       u32       body length in bytes (cap below)
body         length bytes
```

The frame is little-endian. The body encoding is versioned with the frame. Version 1 uses the same field model as the existing `*StateV1` types, encoded as a compact binary (bincode-style, fixed field order). JSON is kept only for debug dumps.

**On the control socket**, a part travels as one or more `PaneSnapshotPart` responses. Each carries `part_index`, `kind`, `chunk_index`, `chunk_count`, `part_bytes` (the whole frame's length) and `data`. `data` is at most 128 KiB of the frame, sent as a JSON byte array, as `PaneStateChunk.data` is sent today (`crates/prismattyc-mux/src/control.rs:1563-1570`, `PANE_STATE_CHUNK_BYTES` at `control.rs:56`). A JSON byte array costs up to four bytes per byte, so one response stays near 512 KiB. That is well under `MAX_RESPONSE_BYTES` (9 MiB, `control.rs:50`). The limit matters for image parts: an 8 MiB image (`MAX_FRAME_BYTES`, `crates/prismattyc-emulator/src/graphics/mod.rs:18`) could never fit in one response.

The reader reassembles chunks in `chunk_index` order. It rejects a part if a chunk index repeats or is missing, if `chunk_count` changes, or if the reassembled length differs from `part_bytes`. It also rejects one whose `part_bytes` exceeds the part's cap before allocating anything. Files store whole frames; chunking is only for the socket.

### Screen part (kind 1)

Everything needed to paint the first frame and to resume the output stream:

- **Cut identity:**
  - `pane_id`, and `through_seq`, the pane-log sequence at the cut. Output streaming resumes after it.
  - `scrolled_lines`. Image placements anchor to absolute primary lines derived from it (`anchor_abs_line`, `crates/prismattyc-emulator/src/graphics/mod.rs:99`, `:126-134`), as do cell-rect anchors. A replica must continue from the same value.
  - `history_generation`, which is bumped whenever history is rewritten (`CSI 3 J`, reflow, import).
  - `history_end`, the absolute index one past the newest history row at the cut.
  - `history_start`, the absolute index of the oldest row still held.
- **Grid:** columns, rows, both grids (rows, cursor, saved cursor, margins, wrap state), `alternate_active`, `origin_mode`, `autowrap`.
- **Modes:** mouse, keyboard stacks and `modifyOtherKeys`, bracketed paste, focus reporting, cursor keys, cursor visibility, cursor shape and blink, `collects_apc`, `retain_alt_history`, `alt_history_rows`.
- **History limits:** `max_scrollback` and `max_scrollback_bytes`, so a replica evicts at the same point as the server.
- **Other state:** cwd, cell pixel size, `full_clears`, `content_epoch`, and the active hyperlink.
- **Tables:** its own style and hyperlink tables, holding only the entries the visible grids use. Grapheme clusters are inlined in the row text, as `RowStateV1` already does.
- **Images:** placement metadata (id, image number, placement id, z, size in cells and pixels, anchor) and virtual placements, each referring to an image by `image_id`. It also carries the graphics `next_id`, which must be non-zero (`GraphicsState::import_state`, `graphics/mod.rs:259-262`). It includes no pixel data. Today a placed image carries its own pixels in `PlacedImageStateV1.rgba` (`graphics/mod.rs:89-100`), and the registry carries the uploaded data. In this format both come from image parts.

Importing a screen part needs a change to the graphics import. Today `import_state` rejects a placement or virtual placement whose image is not in the registry (`graphics/mod.rs:294`, `:312`). It also checks `retained_bytes` against the registry it was given. The screen-part import must accept a placement whose image part arrives later, or was dropped under a cap. That placement paints empty until its image arrives, as an evicted image does today. Retained bytes are then counted from the images actually received. This change is in the build table below.

### History chunk (kind 2)

Fields: `history_generation`, `first_row` (absolute index), `row_count`, the rows (text, style runs, hyperlink cells, wrapped flag, wrap padding), and its own style and hyperlink tables.

- **Chunk size:** 256 rows, or 1 MiB of body, whichever comes first.
- **Order:** in version 1, chunks are contiguous and run from newest to oldest. Each starts where the previous one ended, at `first_row + row_count` of the next older chunk. `ReadPaneSnapshot`, `ReadPaneHistory` and unpark all deliver them that way. The prepend API relies on it and needs no staging buffer. A reader that gets a chunk that does not join the rows it already holds discards it and requests that range again. Arbitrary order would need a staging buffer, which version 1 does not have.

### Gap (kind 3)

Fields: `history_generation`, `first_row`, `row_count`, and a reason:

- `evicted`: the ring dropped the rows before they were fetched;
- `cleared`: `CSI 3 J`;
- `budget`: the rows exceeded the reader's cap;
- `unavailable`: anything else.

A gap tells the reader that history is missing, not short.

### Image part (kind 4)

Fields: `image_id`, format, pixel size, and the decoded or encoded payload as the graphics store holds it.

- **Caps:** one image per part, with the per-image cap the emulator already enforces (8 MiB). The screen part's referenced images total no more than the 16 MiB generation cap.
- **Why separate:** a parked file can keep or drop images under its own size cap without rewriting the screen part. A reader that lacks an image paints the placement as empty, as the host does for an evicted image today.

### Trailer (kind 5)

Fields: total parts, `history_start` and `history_end` as finally written, part offsets (for files), and a CRC32 over the plaintext bytes of the preceding parts' frames.

- **Offsets:** in a file, the offsets point at the stored form of each part. Under an envelope that is the sealed part, so a reader can seek to any part without opening the others.
- **CRC32:** it covers plaintext, so re-sealing parts under another key does not rewrite the trailer.
- **Authentication:** in a parked file the trailer is itself a sealed part, as every part is (see "Parking and at-rest"). Its counts and offsets are therefore authenticated, not just checksummed.

- **Files:** a file without a valid trailer is incomplete. A reader can still use any whole parts before the truncation point.
- **Streams:** the trailer ends the history stream.

### Absolute history addressing

The core screen gains one counter, `history_pushed`: the number of rows ever appended to history in this generation. It counts primary and retained alternate-screen rows alike.

- The oldest held row has absolute index `history_pushed - scrollback.len()`.
- A row keeps its index until it is evicted.
- `CSI 3 J` and reflow start a new generation and reset `history_pushed` to the number of rows they leave.
- `scrolled_lines` stays as it is; it means something else (cell-rect and selection translation).

### Versioning and compatibility

- The parts version starts at 1 and is independent of the control protocol and the existing `EMULATOR_STATE_FORMAT_VERSION`.
- A reader rejects a part with an unknown major version.
- New optional fields are added only at the end of a body. Version 1 readers ignore trailing bytes in a body they understand.
- `ReadPaneState` and `EmulatorStateV1` stay unchanged for old clients. Converting the two-part snapshot to and from `EmulatorStateV1` is defined and tested: screen part plus all history chunks ⇔ one state.

### Size caps

| Item | Cap |
|---|---|
| Part body | 8 MiB, except an image part, which may carry its 8 MiB payload plus a header |
| Screen part, excluding images | 2 MiB |
| History chunk | 256 rows or 1 MiB |
| Total for a pane | the existing 256 MiB `MAX_PANE_STATE_BYTES` |

## Cut procedure

1. Take the plane lock and find the pane. If the parser is mid-sequence, retry at the next boundary, as `ReadPaneState` does today.
2. Still under the lock, build the screen part: visible grids, modes, used styles and hyperlinks, `through_seq`, and the history counters. This measured at well under a millisecond.
3. Release the lock and encode, then send the screen part, then a Ready marker.
4. Serve history in pages. Each page takes the lock briefly, exports at most 256 rows by absolute index from `history_end` backwards, and releases it before encoding. A 256-row page costs about 0.6 ms under the lock at the export rate above. No lock is held between pages.

### Consistency

The screen part is a true cut, because it comes from one lock hold at a parser boundary. History rows below `history_end` never change while their generation lasts. So paging them over several lock holds gives the same rows a single hold would have. Each possible change is handled:

- **New output after the cut:** this only appends rows at or after `history_end`. The reader receives those through the output stream from `through_seq` and never asks for them as history.
- **Ring eviction:** a requested row older than the current `history_start` comes back as a gap with the reason `evicted`.
- **`CSI 3 J`:** the generation changes. The next page reply is an error naming the new generation. The reader drops the history it fetched for the old generation, keeps its screen, and treats history as cleared. That matches what the pane shows.
- **Resize (reflow):** the generation changes the same way. The reader can stop or start again from a new screen cut. In both cases the output stream from `through_seq` already carries the resize, so the screen stays right.
- **Alternate-screen switch:** this does not rewrite history, so it needs nothing special. Rows the alternate screen pushes are counted by `history_pushed` like any other row.

If paging under short lock holds proves awkward to build, the fallback is to clone the history rows under the lock (measured at about 2.4 ms for 10,000 rows of 200 columns) and encode them after it. Copy-on-write history chunks would make even that clone close to free. Neither changes the format.

## Attach protocol

New control requests, served on the client thread like `ReadPaneState`:

- **`ReadPaneSnapshot { pane_id, history: none | recent(rows) | all }`:** streams `PaneSnapshotPart` responses: the screen part, its image parts, then `Ready`, then history chunks and gaps from newest to oldest (as many as asked for), then the trailer.
- **`ReadPaneHistory { pane_id, history_generation, before_row, max_rows }`:** returns chunks and gaps for one range, for clients that page on demand. A generation mismatch is a typed error that names the current generation.

Grants are the same as `ReadPaneState` and `ReadPaneStyled`: an active client on this socket. No pane text goes anywhere it does not go today.

### Connections

A control connection serves one request at a time, to completion (`handle_client_loop`, `control.rs:8278`; the `ReadPaneState` branch at `:8485-8505`). The host's pane reader today runs `ReadPaneState` and then `SubscribePane` on the same connection (`crates/prismattyc-host/src/attach_log.rs:1472-1493`, `:1216`). The server accepts at most `MAX_CLIENTS = 64` connections (`control.rs:62`). So:

- **Pane reader connection (one per open pane, as today):** sends `ReadPaneSnapshot { history: none }` or a small `recent(N)` that fits the first scroll screen. As soon as the trailer arrives, it sends `SubscribePane { from_seq: through_seq }` on the same connection. Output is never held back behind history.
- **History connection (one per host, shared by all panes):** older history is paged with `ReadPaneHistory` on a separate connection that the host shares across panes and serves from one queue. Pages are taken newest first for the pane being looked at, then in the background for the rest. Paging therefore adds one connection per host, not one per pane, and does not multiply the 64-client limit.
- **`ReadPaneSnapshot { history: all }`** is for single-shot readers: tools, and the parking writer inside the server. An interactive host does not use it.

### Clients

- **GPU host:**
  - Tries `ReadPaneSnapshot` first, and falls back to `ReadPaneState` when the server rejects the request (the same pattern attach uses for `view_offset` today).
  - Imports the screen part into an emulator with empty history, paints, and subscribes from `through_seq` on the same connection.
  - Pages older history over its shared history connection, and prepends the chunks in order. This needs one new core screen API, which inserts older rows at the front within the row and byte caps and ignores them if the generation has moved on.
  - Hyperlinks: each chunk has its own small table, so the prepend maps its links into the screen's table and merges duplicate targets. The screen holds at most `MAX_HYPERLINKS = 4096` (`crates/prismattyc-core/src/lib.rs:178`, enforced at `:1266`). Once the table is full, a prepended cell keeps its text and loses its link, which is what the screen does today with a new link at the cap.
- **`pmux-attach`:** no change needed. It can later use `ReadPaneHistory` for scroll mode instead of a full styled read per screen of history.
- **Old pairings:** an old host on a new server keeps using `ReadPaneState`. A new host on an old server falls back.

## Parking and at-rest

Parking writes this format to disk. The design is in `docs/design/park-idle-panes.md`. This section sets down what the format guarantees a parked file and what the store adds.

**File layout.** A parked file is the screen part, then history chunks and gaps from newest to oldest, then image parts, then the trailer. Every part is sealed separately. A new client's first frame comes from the screen part alone. In practice a parked pane keeps its screen in memory, so the file is read only for history.

**Gaps.** Rows evicted between the cut and the page that would have written them go in as gap parts with the reason `evicted`. A pane restored from the file knows its history is missing there, not short.

**Envelope.** The plaintext parts carry no key fields. A store seals each part in an envelope:

```
key_id     u32    which key sealed it (the per-run key for parking)
nonce      12 B   file id (8 B) + part index (u32)
ciphertext length bytes, ChaCha20-Poly1305
tag        16 B
```

The authenticated data is the store's file header (pane id, file id) plus the part's own frame header. A part cannot be moved to another file or another position without failing to open. Because the envelope wraps whole parts, a store with a different key can re-seal parts without re-encoding them. Parking does not need to: restore after exit uses the plaintext checkpoint written just before the pane parked, and parked files are deleted at exit.

**Trailer.** A parked file is complete only with a valid trailer. A file cut short by a crash still yields its whole parts. Since parking writes the trailer before it frees any history, a pane is never left depending on a file without one.

**Images.** Image parts follow the history and count toward the store's cap. If a parked file would exceed it, the park writes no image parts, and the screen part's placements paint empty after unpark, as for an evicted image.

**Output while parked.** Output unparks the pane, and the runtime thread remains the only writer:

- In `LivePane::drain`, a parked pane with a chunk waiting stops draining and does not apply it.
- Chunks wait in order in the bounded channel. Once it fills, the 8 MiB byte budget blocks the reader and the kernel holds back the child. No byte is dropped or reordered.
- The park worker decodes the history chunks off the lock.
- On the next tick the runtime prepends them to the screen-only emulator and drains the queue.

No output was applied while the pane was parked, so the generation and `history_end` still match the file, and the prepend is exact.

**`through_seq` and the pane log across a park:**

- **At park:** the pane-log sequence continues and is not reset. The screen part records `through_seq`, and the in-memory log ring is cleared.
- **While parked:** a client that subscribes from an earlier sequence gets a gap and a screen frame, as when a client falls behind today. A client that starts from the screen part subscribes from `through_seq` and misses nothing, because nothing was appended while parked.
- **After unpark:** new output continues the sequence from `through_seq + 1`.

## Security

- **Access:** the snapshot carries the same pane content as `ReadPaneState` and `ReadPaneStyled`, under the same client grant, and adds no new readers.
- **Storage:** files that store parts are the storing feature's responsibility: owner-only permissions, the envelope above and size caps.
- **Decoding:** caps apply on decode as well as encode. A malformed or oversized part is rejected without allocating past the cap.

## Risks

- **Prepending history in the host emulator is new core screen code.** Cluster handles, the hyperlink table, the byte budget and reflow all have to respect rows that arrive late. Tests must cover a resize while history is still arriving.
- **The counter has to be right everywhere.** `history_pushed` must be maintained at every site that pushes to, evicts from, clears or reflows history. There are only a few, but a miss would silently mislabel rows. The page export should assert the invariant in debug builds.
- **Image data can dwarf history.** Keeping it in separate parts makes the cost visible. Readers must treat a missing image as normal.
- **The cut still runs under the shared lock.** A screen cut is cheap, but pages under the lock add up on very busy servers. The page size is the dial. Moving history to copy-on-write chunks is the longer-term fix, and it would also help the checkpoint path.

## Recommendation

**Go.** The current state already splits naturally into screen and history. The screen cut measures at about 0.03 ms against 22 ms for today's full export. Absolute addressing needs one new counter and a generation number, not a storage rewrite. Every change between the cut and a history fetch can be made explicit.

### Build size

| Piece | Agent-hours |
|---|---|
| Format types, encoder and decoder, `EmulatorStateV1` conversion, caps, socket chunking and reassembly | 4.5 to 6 |
| Core screen: `history_pushed`, generation, page export by absolute range, prepend API with hyperlink remapping at the cap (the parking work uses the same prepend API, so it is counted here only) | 3.5 to 4.5 |
| Graphics import: accept placements whose image parts arrive later or were dropped, `next_id`, retained-bytes accounting | 1 to 2 |
| Control requests (`ReadPaneSnapshot`, `ReadPaneHistory`), short lock holds, typed generation error | 2 to 3 |
| GPU host: screen-first restore, then subscribe on the same connection; one shared history connection for every pane; background paging; fallback | 4 to 5 |
| Tests: cut consistency, eviction gaps, clear and reflow generations, chunk reassembly and caps, missing images, hyperlink cap, old/new pairings, a lock-hold bound | 2.5 to 3.5 |
| Proofs: large-history attach before and after (time to first frame, lock hold), on macOS and Linux | 1 to 2 |
| **Total** | **18.5 to 26** |

That is about 4 to 5 wall-clock days with review rounds, CI, and clean-machine proofs. It is above the 12 agent-hours planned for two reasons:

- the core screen counter and the host's history prepend, which only became visible once the code was mapped;
- the review's additions, which add 3.5 to 5 hours: socket chunking for large parts, the graphics import change, the shared history connection, and hyperlink remapping.
