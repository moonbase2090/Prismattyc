# Kitty graphics decoder for prism-emulator + prism-host

- **Status:** Design, approved for spec review
- **Date:** 2026-08-20
- **Author:** Brandan Majeske
- **Related:** `docs/kitty-graphics-rich-experience-spike.md` (the rich-asset
  spike — a *separate* workstream; this doc is the "classic Kitty
  compatibility" workstream that spike explicitly deferred, §"Classic Kitty
  compatibility remains separate")

## Problem

Inline images from a PTY child do not render in prism-host. A user runs Claude
Code in prism-host; Claude Code shows a text placeholder instead of its logo.
The same session renders the logo in Ghostty and Kitty.

## Root cause

Claude Code emits the **Kitty graphics protocol** and negotiates capability
before it sends any pixels:

1. It sends a query: `ESC _ G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC \`
   followed by primary device attributes `ESC [ c`.
2. It sends the image only if it receives `ESC _ G i=31;OK ESC \`, **or** if
   `$TERM` / `$TERM_PROGRAM` is in its allowlist (`kitty`, `ghostty`,
   `wezterm`).

prism-emulator forces `TERM=prism-direct` (fallback `xterm-256color`), strips
`KITTY_*` from the child environment on purpose
(`prism-emulator/src/lib.rs:950-973`), and never answers the graphics query.
Claude Code therefore decides the terminal has no graphics support and prints
its own placeholder. **prism never receives image bytes.**

Confirmed by direct inspection of the Claude Code binary (v2.1.237): it emits
`ESC _ G a=T,t=d,f=100,q=2;<base64 PNG> ESC \` for small inline images and
`ESC _ G a=T,t=f,f=100,q=2;<base64 path> ESC \` for larger ones. Format is
always PNG (`f=100`); no Sixel path exists; no zlib (`o=z`); no Unicode
placeholder (`U=1`).

## Goal

Render inline Kitty-graphics images from a well-behaved PTY child in
prism-host, on both the direct-host and mux-attached paths, without weakening
Prism's isolation model or its quiet-idle (no-idle-tax) behavior.

## Non-goals

- Sixel and iTerm2 OSC 1337 (evaluate separately if evidence needs them).
- Animation (`a=a` frames), Unicode/virtual placement (`U=1`, U+10EEEE), and
  tmux pass-through relay.
- Raw `f=32`/`f=24` transmission and `o=z` zlib payloads (Claude Code sends
  PNG only; add later only if a real producer needs them).
- POSIX shared-memory transport (`t=s`).
- The rich-asset plane (`rich.image.rgba.v1`) from the spike — that is a
  distinct Prism-originated feature, not this child-emitted decode path.

## Chosen approach (A): decode at the emulator boundary

Parse and decode inside `prism-emulator`. Hold decoded RGBA plus cell
placement in a new image store **on `Emulator`** (not on `prism-core::Cell` /
`Screen`, which are `Copy` + `PartialEq/Eq` and must stay decoder-free). Host
and mux read a new `Emulator::images()` accessor and blit through the existing
`raster::blit_rgba` compositor with scaling added.

Rationale: smallest blast radius; `prism-core` is untouched; both render paths
already drive one `Emulator` per pane, so direct + mux parity and mux
detach/reattach come almost for free (the mux server already owns the
emulator). Rejected: (B) store encoded PNG and decode per-viewer — duplicate
decode, awkward server ownership; (C) separate rich asset plane — built for
Prism-originated artifacts, overkill for decoding a child's `_G` at the cursor.

### Data flow

```text
PTY child (Claude Code)
  | ESC _ G <control>;<base64> ESC \      (query, transmit, delete)
  v
prism-emulator :: Emulator::feed(bytes)
  | 1. graphics APC scanner reassembles m= chunks -> full control + payload
  | 2. a=q  -> queue "ESC _ G i=<id>;OK ESC \" into pending_replies
  | 3. a=T  -> base64 decode -> PNG decode (bounded) -> RGBA
  |            -> store {id, placement(row,col,anchor_line), rgba, w,h, cells}
  | 4. a=d  -> scoped delete by image id
  v
Emulator::images() -> &[PlacedImage]       (new read accessor)
  v
prism-host :: rasterize_frame (main.rs:1139)     mux server owns Emulator;
  | after pane screen paint, for each visible    viewer reads images() the
  | image: cell rect -> scale RGBA -> blit_rgba   same way (parity)
  v
softbuffer / gpu framebuffer (u32 0RGB)
```

## Protocol subset

Framing: APC `ESC _ G` ... `ESC \` (`0x1b 0x5f 0x47` ... `0x1b 0x5c`).
Control data is `key=value` pairs, comma-separated, then `;`, then base64
payload.

First cut MUST support:

| Key | Values supported | Notes |
|-----|------------------|-------|
| `a`  | `T` transmit+display, `q` query, `d` delete | ignore `t`/`p`/`f` frame actions |
| `f`  | `100` (PNG) | reject `24`/`32` in first cut (log + drop) |
| `t`  | `d` direct base64; `f` regular file | see security below |
| `m`  | `0`/`1` chunking | reassemble across successive APCs |
| `i`  | image id (u32) | echoed in query reply and delete scope |
| `q`  | `0`/`1`/`2` quietness | suppress OK/error replies per level |

Placement in the first cut is **classic direct placement**: the image lands at
the cursor cell at transmit time and occupies a computed cell span. `c=`/`r=`
(explicit cols/rows) and `x=`/`y=` pixel offsets are honored if present, else
derived from image pixel size and `FontMetrics.cell_w/cell_h`.

### Negotiation (fail-closed)

Answer the query, do not rely on the TERM allowlist (Prism deliberately hides
terminal identity). On `a=q`: validate the tiny probe, then queue
`ESC _ G i=<id>;OK ESC \` into `Emulator.pending_replies`
(`prism-emulator/src/lib.rs:163`), drained by `take_pending_replies`
(`:284`) — the same channel the Kitty *keyboard* query reply already uses
(`:428`). Respect `q`: `q>=1` suppresses the `OK`; errors are suppressed at
`q>=2`. If we cannot honor a request, send no image data — never a partial
frame.

## Emulator changes (`prism-emulator`, `prism-protocol`)

### APC intake

The current `ApcCollector` (`prism-protocol/src/lib.rs:2736`) blocks `_G`:
`MAX_CONTROL_BODY_BYTES = 4096` and printable-ASCII-only, `Prism;`-namespaced,
so `_G` bodies are returned `Discarded` and dropped by the host `Err(_)` arm
(`prism-host/src/rich.rs:841`). Do not widen the Prism collector. Add a
**separate graphics APC scanner** that recognizes a leading `G` after
`ESC _` and accumulates that sequence under graphics limits (chunked payloads
routinely exceed 4096 B total; each on-wire chunk is <=4096 B base64, and
`m=1` continuations arrive as successive APCs to be reassembled). Feed order in
`Emulator::feed` (`lib.rs:299-334`) stays: sidecar scanner sees bytes, then
`parser.advance`.

### Image store on `Emulator`

New field, e.g. `graphics: GraphicsState`, holding:

- `images: Vec<PlacedImage>` where `PlacedImage { id, rgba: Arc<[u8]>, w, h,
  cols, rows, anchor_abs_line: u64, anchor_col: u16, epoch }`.
- Reassembly scratch keyed by image id for in-flight `m=` chunks.
- Per-generation byte accounting for the memory cap.

`Arc<[u8]>` so a viewer can hold RGBA cheaply. `anchor_abs_line` is an absolute
line index derived from `screen().scrolled_lines()` at transmit time; the same
delta trick `rich.rs` uses to reanchor cell-rect attachments keeps images
pinned to their text as the screen scrolls, and lets them scroll off into
history and be dropped.

### Read accessor

`Emulator::images(&self) -> &[PlacedImage]` (mirrors `screen()` at `:268`),
consumed by host and mux. `content_epoch` continues to drive repaint; bump/tag
images with the current epoch so a stale placement is easy to skip.

## Decode + bounds (security-critical)

Decode in `prism-emulator` (add `png = "0.17"` — already a workspace lock
entry; base64 decode hand-rolled ~30 lines to match the no-dep-CLI style, or a
small `base64` dep — decide in the plan).

Order and checks:

1. Base64-decode the reassembled payload. Reject non-base64 bytes.
2. For `t=f`: resolve the path **safely** (see below) and read the file.
3. Decode PNG with the `png` crate, but read the IHDR first and **reject before
   allocating** if `width * height * 4` exceeds the byte cap or dimensions
   exceed the max. This is the decompression-bomb defense the spike requires.
4. Overflow-check every `width * height * bpp`, stride, and offset computation
   (use checked arithmetic).

Advertised/enforced limits (prototype values, tune in the plan):

- Max image dimensions: 2048 x 1024 px.
- Max decoded frame: 8 MiB.
- Max retained raster per PTY generation: 16 MiB (evict oldest / drop new).
- One in-flight reassembly and a bounded number of live images per pane.

`t=f` file-transport validation (the surface the spike flags):

- Open the path, then `fstat` the **open fd** (no path re-lookup) to defeat
  TOCTOU.
- Require a regular file (reject symlink target that is a device/socket/dir).
- Owner must be the current uid.
- Enforce the size cap on the fstat result before reading.
- Never follow into a path outside expectations; treat the whole file as
  untrusted input.

On any validation failure: drop only that image, record one bounded warning,
keep the child, grid, and other images alive. Rate-limit repeated invalid
commits.

## Host rendering (`prism-host`)

Blit site: `rasterize_frame` (`main.rs:1139`), immediately after the pane's
`rasterize_screen_at_with_theme` call (`main.rs:1233-1246`), inside the pane's
clip box so images land on top of the text grid but never escape the pane.

For each visible `PlacedImage`:

1. Compute the destination cell rect: `x0 = origin_x + col*cell_w`,
   `y0 = origin_y + (row - scroll)*cell_h`, size `cols*cell_w x rows*cell_h`
   (mirrors `raster.rs:770-771`). Skip if fully scrolled out or clipped to
   zero.
2. Scale the source RGBA to the destination rect. `blit_rgba`
   (`raster.rs:2113`) currently maps 1:1 with no scaling — add nearest-neighbor
   (or simple box) scaling, or pre-scale into a temp buffer, then alpha-blend
   with the existing over-blend and clip logic.
3. Present through the existing softbuffer / gpu path unchanged
   (`main.rs:516-541`).

Quiet-idle: images are static, so no timer. A new/changed/deleted image sets
`host.dirty` and requests a redraw through the existing PTY-drain path
(`main.rs:904-958`); idle stays `ControlFlow::Wait` (`main.rs:616`). No polling.

## Mux (`prism-mux`) — stage 3

The mux server already owns the `Emulator` per pane and mirrors
`process_rich_chunk` / `feed_rich_slice` / `handle_control_events`
(`prism-mux/src/rich.rs:684-735`). Because RGBA lives on the server's
`Emulator`, a newly attached viewer reads `images()` and paints the current
images with no task rerun. Keep scale/clip/visibility viewer-local; one viewer
must not mutate the shared store. Invalidate the whole store when the PTY
generation changes. Byte-parity gate: the same session must produce equivalent
pixels through the direct host and a mux viewer.

## Cleanup + isolation

- `a=d` deletes scoped by image id only (never a global "delete all").
- Invalidate all images on: screen clear, alternate-screen enter/leave, resize,
  and PTY-generation change.
- Per-generation memory cap enforced; oldest images evicted first.
- A pane's images are keyed to that pane's emulator — no cross-pane reach.

## Staging

1. **Direct host, `t=d` inline + `m=` reassembly + PNG + query `OK`** — renders
   the Claude logo end to end. (Primary bug fix.)
2. **`t=f` file transport** with the full validation set above.
3. **Mux server ownership + reattach + per-viewer projection**, direct/mux
   byte-parity.
4. **Cleanup / isolation** hardening: scoped `a=d`, invalidation triggers,
   memory cap, rate limiting.

## Testing + acceptance gates

Unit (prism-protocol / prism-emulator):

- APC graphics framing parse; `key=value` control parse; unknown-key ignore.
- `m=` chunk reassembly across successive `feed` calls; oversized/overflowing
  reassembly rejected.
- Base64 decode incl. malformed input.
- Query `a=q` -> exactly one `ESC _ G i=<id>;OK ESC \` in `pending_replies`;
  `q>=1` suppresses it.
- Bounds: dimension, byte-length, and `w*h*bpp` overflow all fail closed
  **before** allocation.
- `t=f`: reject symlink-to-device, wrong owner, oversize, dir, socket;
  TOCTOU (file swapped between open and fstat) fails closed.
- Delete `a=d` scoped to id; clear/alt/resize/generation invalidation.

Integration / visual:

- Direct host renders a known PNG fixture at the expected cell rect; scaling
  correct; stays within the pane clip.
- Direct vs mux-viewer pixel parity for the same fixture.
- Detach/reattach restores the image without rerunning the child.
- Quiet-idle: a static image adds no timer wake; idle stays `Wait`.
- End-to-end: Claude Code logo renders in prism-host (manual acceptance).

Gate summary (from the spike, applied here): query-first fail-closed; bounds
fail closed; file isolation; PTY progress never blocked by decode; quiet idle;
geometry stays in-pane; direct/mux parity; detach/reattach; scoped cleanup.

## Open questions (resolve in the plan)

- Base64: hand-rolled decoder vs a `base64` crate dependency.
- Exact memory-cap numbers and eviction policy under many panes / detached
  viewers.
- Scaling quality: nearest-neighbor first, or box/bilinear for the logo?
- Do we also need `t=t` (temp file, delete-after-read)? Claude Code uses `t=f`;
  confirm whether it ever uses `t=t`.
- Should image placement reanchor exactly like `AttachCellRect`, or is a
  simpler absolute-line pin sufficient for the first cut?
```
