# Kitty Unicode Virtual Placement — Design Spec

**Status:** Draft for review — 2026-08-20. Review notes:
[2026-08-20-kitty-unicode-placement-review.md](2026-08-20-kitty-unicode-placement-review.md)
(8 bugs, 3 suggestions; open).

**Goal:** Render images that producers (Claude Code's footer mode-icons, and any
Kitty-graphics app) place via the Kitty **Unicode placeholder** mechanism
(`U+10EEEE` cells), so they display as real pixels instead of tinted glyph boxes.

**Scope decision:** Full Kitty spec (not minimal-for-Claude), per owner. This
includes image-id most-significant-byte and placement-id addressing, which
requires an underline-color core prerequisite.

## Background

Prism already decodes and renders Kitty graphics for **direct placement**
(`a=T` at the cursor with `C=1`) — that path draws Claude Code's big logo. See
`crates/prism-emulator/src/graphics/` and the blit loop in
`crates/prism-host/src/main.rs` (`rasterize_frame`).

**Direct placement** anchors an image at the cursor cell. **Unicode virtual
placement** is different: the image is transmitted (often transmit-only), and
the producer then writes a grid of `U+10EEEE` placeholder cells into the normal
text stream. Each placeholder cell carries the image id (in its foreground
color) and its row/column within the image (as combining diacritics). The
terminal draws the corresponding sub-rectangle of the image over each cell.
Because placeholders are ordinary grid cells, they scroll, reflow, and get
overwritten with the text — the terminal need not track placement geometry.

Today Prism's parser (`graphics/command.rs`) handles only `a/t/f/m/i/c/r/q`.
`U=`, transmit-only `a=t`, and placement id `p=` are ignored; `U+10EEEE` cells
are drawn as fallback glyphs (the "blue box" tinted by the id-bearing fg color).

## Encoding rules (Kitty spec, verbatim intent)

- **Placeholder codepoint:** `U+10EEEE`.
- **Image id, low 24 bits:** the cell **foreground color**. In 24-bit color the
  RGB bytes are the three low bytes of the id (R = bits 16–23, G = 8–15,
  B = 0–7). In 256-color mode the palette index is the low byte.
- **Image id, most-significant (4th) byte:** the **third** combining diacritic
  on the cell (optional; absent → 0).
- **Placement id:** the cell **underline (decoration) color** (SGR `58`).
  Absent/zero → the terminal may pick any virtual placement of that image.
- **Row / column:** the **first** diacritic encodes the row index, the
  **second** encodes the column index, drawn from Kitty's ordered
  `rowcolumn-diacritics` table (`U+0305`→0, `U+030D`→1, `U+030E`→2, …; 297
  entries). Diacritic order is row, then column, then id-MSB.
- **Omitted diacritics (run continuation):** a placeholder cell with the **same
  fg + underline color** as the preceding placeholder cell and no explicit
  row/column inherits `row = previous row`, `column = previous column + 1`
  (left-to-right). A new row is started by an explicit row diacritic. This lets
  producers emit one diacritic pair at the run start and bare `U+10EEEE`
  afterward.
- **Transmission:** the image is created before/with the placeholders via
  `a=T,U=1,i=<id>,c=<cols>,r=<rows>[,q=2]` (transmit-and-virtual-place) or
  transmit-only `a=t,i=<id>,f=100,...`. `c`/`r` give the intended cell footprint;
  the image is fit to that rectangle preserving aspect ratio.

## Architecture

**Chosen approach: id-keyed image registry + paint-time cell scan.** Mirrors
Kitty. Rejected: resolving placeholders into fixed placements at feed time
(breaks under scroll/edit; fights the grid model).

Data flow:

1. **Transmit** (`a=t`, or `a=T,U=1`): decode PNG (existing
   `decode_png_bounded`), store the raster in an **id-keyed registry**. Do NOT
   create a cursor placement.
2. **Text stream:** the emulator writes `U+10EEEE` cells with fg color +
   diacritics + underline color into the `Screen` exactly like any glyph
   (already supported: `Cell.character`, `combining`, `foreground`; underline
   color is the new prerequisite).
3. **Paint:** a second pass in `rasterize_frame` scans visible cells, finds
   `U+10EEEE` runs, decodes (id, placement id, per-cell row/col), looks up the
   raster by id, and blits the mapped source sub-rectangle into each cell.

### Component 1 — Core prerequisite: underline color (prism-core + emulator)

- **`Style`** (`crates/prism-core/src/lib.rs`): add
  `pub underline_color: Color`. `Color::Default` means "no explicit decoration
  color" (placement id 0).
- **`Cell`**: carries `Style`, so it gains the field transitively; ensure it is
  copied on writes and reset on `erase`.
- **SGR parser** (emulator): handle `58` (set underline color:
  `58:2::r:g:b` / `58:2:r:g:b` truecolor, `58:5:idx` indexed) and `59`
  (reset underline color to default). Unknown `58` subforms → ignored, no panic.
- **Interface produced:** `Cell::underline_color() -> Color` (or public field)
  for the paint scan.

### Component 2 — Parser (`graphics/command.rs`)

- Add fields to `GraphicsCommand`: `unicode_placement: bool` (`U=1`),
  `placement_id: u32` (`p=`). Keep existing fields.
- Extend `Action`: treat lowercase `a=t` as **transmit-only** (currently maps to
  `Action::Other` and is dropped). Model as `Action::Transmit` plus a
  `place: bool` derived from `a` (`T` → place at cursor; `t` → registry only).
- `U=1` with `a=T` means transmit + virtual placement: store in the registry and
  do **not** create a cursor placement.

### Component 3 — Image registry (`graphics/mod.rs`)

- Separate raster from placement. Introduce:
  ```rust
  pub struct StoredImage {
      pub id: u32,
      pub rgba: Arc<[u8]>,
      pub width: u32,
      pub height: u32,
      pub cols: u16,   // intended cell footprint (from c=)
      pub rows: u16,   // intended cell footprint (from r=)
  }
  ```
- `GraphicsState` keeps: `registry: Vec<StoredImage>` (id-keyed; by-id replace),
  the existing `images: Vec<PlacedImage>` for direct placements (unchanged
  behavior), and `retained_bytes` counting **both**. The 16 MiB
  `MAX_RETAINED_BYTES` cap and oldest-first eviction cover the registry too.
- `pub fn image_by_id(&self, id: u32) -> Option<&StoredImage>` for the host.
- Direct placement continues to push `PlacedImage` **and** upserts the registry,
  so a later virtual placement of the same id can find the raster.
- **Deletion** (`a=d`): `d=A/a` clears both `images` and `registry`; `i=<id>`
  removes the placement and the registry entry for that id.

### Component 4 — Placeholder decode (`graphics/placeholder.rs`, new)

- `const PLACEHOLDER: char = '\u{10EEEE}';`
- Static `ROWCOLUMN_DIACRITICS: &[char]` — the 297-entry Kitty table, generated
  verbatim from kitty's `rowcolumn-diacritics.txt`. `fn diacritic_index(c: char)
  -> Option<u16>` via binary search or a match.
- `fn image_id_low24(fg: Color) -> Option<u32>` — RGB → 24-bit; indexed → low
  byte. `Color::Default` → None (not a placeholder image).
- Pure, unit-tested; no I/O.

### Component 5 — Host paint (`crates/prism-host/src/main.rs`)

New pass in `rasterize_frame`, after text raster, before/after the existing
direct-placement blit (order: direct placements, then virtual — or by z; keep
simple: direct then virtual):

```
for each visible cell (row, col):
    if cell.character != U+10EEEE: reset run state; continue
    id_low = image_id_low24(cell.foreground)?          // skip if none
    id = id_low | (msb_diacritic << 24)
    (r, c) = decode row/col diacritics, else continuation default
    img = graphics.image_by_id(id)?                    // skip if unknown
    // source sub-rectangle for cell (r,c) of an img.cols×img.rows grid:
    src_x = c * img.width  / img.cols
    src_y = r * img.height / img.rows
    src_w = img.width  / img.cols
    src_h = img.height / img.rows
    blit_rgba_scaled(sub-rect of img.rgba -> this one cell rect, clipped)
```

- The per-cell dst rect is exactly one terminal cell (`cell_w × cell_h`) at the
  cell's pixel position; scaling reuses `blit_rgba_scaled`.
- Runs are grouped only to carry the continuation default (previous row/col);
  each cell still blits its own sub-rect. Bounded by visible cell count — no
  attacker-controlled unbounded loop (fail-closed constraint preserved).
- Contiguous cells of the same id/placement tile the image; gaps/overwrites are
  handled naturally because absent placeholder cells simply draw nothing.

### Component 6 — Lifecycle / invalidation

- Registry cleared alongside `images` on full clear, alt-switch, and resize
  (reuse existing `GraphicsState::clear()` calls in emulator `feed`/`resize`).
- Reattach (mux) persistence: registry lives in the server-owned emulator like
  `images` today; no viewer state.

## Bounds / fail-closed (Global Constraints)

Inherit the merged decoder's guarantees, extended to the registry:
- PNG decode caps dims/bytes before allocation; APC accumulation cap
  (`MAX_GRAPHICS_APC_BYTES`); per-generation retained cap (16 MiB) counts
  registry + placements with oldest-first eviction.
- Paint scan iterates only visible cells → bounded work per frame regardless of
  child input. No per-cell allocation in the hot loop.
- `t=f` file-transport safety unchanged (fstat/owner/size, TOCTOU-safe).
- Per-pane isolation: registry is per-`GraphicsState` (per pane).
- Works by default: no `experimental_rich` gating; virtual placement handled on
  the standard `feed()` path like direct placement.
- Malformed input (bad diacritic, unknown id, missing fg) → cell draws nothing
  or its fallback glyph; never panics.

## Testing

- **Parser:** `U=1`, `a=t` transmit-only, `p=` parse correctly; `a=t` no longer
  dropped.
- **Registry:** transmit-only stores raster by id without a placement;
  `image_by_id` returns it; by-id replace; retained cap evicts; `a=d` by id and
  `d=A` clear registry.
- **Placeholder decode:** diacritic table maps `U+0305/030D/030E`→0/1/2; id from
  fg color (truecolor + indexed); msb from 3rd diacritic; continuation default
  (bare `U+10EEEE` after a seeded cell → next column).
- **Underline color:** SGR `58:2` truecolor, `58:5` indexed, `59` reset; stored
  on the cell; survives copies; reset by `erase`.
- **Host paint (unit):** given a Screen with a known placeholder grid and a
  registry image, the blit maps cell (r,c) to the correct source sub-rect
  (assert a probe pixel). Bounds: a placeholder id with no registry entry draws
  nothing.
- **Manual acceptance:** Claude Code footer mode-icons render as pixels in a
  fresh `prism-host` (crab + footer icons match Ghostty); mux attach.

## Out of scope (first cut; note, don't hide)

- Animation frames, `z=` stacking/compositing beyond simple over-text draw,
  `o=z` zlib payloads, `f=24/32` raw RGBA, `t=t/t=s` transports (already out).
- Non-PNG registry formats.
- Remote mux-attach client re-emitting graphics to its own terminal.

## Global Constraints (carry into the plan)

- Works by default; never gate on `experimental_rich`.
- Fail-closed bounds: no attacker-controlled unbounded work or allocation.
- Per-pane isolation; server-owned registry persists across reattach.
- No `#[allow(dead_code)]` scaffolding left behind.
- ASD-STE100 in prose; normal English in code/commits.
