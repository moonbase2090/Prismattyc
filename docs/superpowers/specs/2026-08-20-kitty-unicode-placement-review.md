# Review: Kitty Unicode virtual-placement spec

**Branch:** `feat/kitty-unicode-placement` vs `origin/main` (`f2996f1`).
**Reviewed:** 2026-08-20 (operator-a). Spec only; no decoder/host code on this branch.
**Counts:** 8 bugs, 3 suggestions, 0 nits. All status open.

## Summary

The paint-time `U+10EEEE` scan and underline-color prerequisite are the right
shape, but the written protocol model does not match Kitty or Ghostty.
Implementing it as specified would miss `a=p` virtual placements, stretch tiles
instead of fitting them, leave the macOS tofu under alpha icons, and ignore
placement-id. Dominant risk is treating an image registry as a substitute for
virtual placements.

## Issues

### Issue 1 — Severity: bug

- File: `2026-08-20-kitty-unicode-placement-design.md:100`
- Description: Component 2 never parses `a=p`. It only adds `a=t` as
  `Action::Transmit` plus a `place: bool` (`T` → cursor place, `t` → registry
  only). Kitty's unicode-placeholder path is transmit with no placement, then
  `a=p,U=1,i=<id>,c=<cols>,r=<rows>` (or combined `a=T,U=1`). Ghostty's tests
  and `placeholderTarget` require that virtual put. `place: bool` on Transmit
  cannot represent put-without-transmit, so `a=p,U=1` stays `Action::Other` and
  is dropped. Parser tests list `U=1`, `a=t`, and `p=` only — no `a=p`.
- Suggestion: Add `Action::Put`. On `a=p,U=1`, create/replace a virtual
  placement `{image_id, placement_id, cols, rows}` and do not cursor-place. On
  `a=p` without `U=1`, keep first-cut ignore (or cursor-place later). On
  `a=T,U=1`, transmit into the registry and create the virtual placement in one
  step.
- Status: open

### Issue 2 — Severity: bug

- File: `2026-08-20-kitty-unicode-placement-design.md:119`
- Description: The spec claims full Kitty, including placement-id via underline
  color, but Component 3 stores `cols`/`rows` on `StoredImage` and Component 5
  looks up `graphics.image_by_id(id)` only. Paint never reads cell underline
  color or command `p=`. Kitty allows several virtual placements of one image,
  each with its own `p=`, `c=`, `r=`. Zero underline color may pick any virtual
  placement of that image; a non-zero id must select that placement. Flattening
  onto the image makes `c`/`r` from a later `a=p,U=1` unable to update a prior
  transmit, and two placements of one id cannot differ.
- Suggestion: Keep the id-keyed raster registry, and add a virtual-placement
  table keyed by `(image_id, placement_id)` holding `cols`/`rows`. Paint: decode
  placement id from underline color; if zero, take any virtual placement for
  that image; then use that placement's grid to map the tile.
- Status: open

### Issue 3 — Severity: bug

- File: `2026-08-20-kitty-unicode-placement-design.md:153`
- Description: The per-cell source rect is `src_x = c * width / cols`,
  `src_w = width / cols` (same for rows). That stretches the image into the
  `c×r` grid and drops `width % cols` / `height % rows` remainder pixels. Kitty
  requires the image be fit into the virtual-placement rectangle with aspect
  ratio preserved. Ghostty's `renderPlacement` letterboxes and centers; its dog
  4×2 fixture does not map cell 0 to `0..width/4` of the raw PNG.
- Suggestion: At paint time, fit the raster into `cols*cell_w` × `rows*cell_h`
  preserving aspect (letterbox), then take the cell's slice of that fitted
  rect. Use inclusive bounds `x0 = c * w / cols`, `x1 = (c+1) * w / cols` so
  remainders are not lost. Guard `cols == 0` / `rows == 0` (Ghostty then derives
  grid from image px / cell px).
- Status: open

### Issue 4 — Severity: bug

- File: `2026-08-20-kitty-unicode-placement-design.md:53`
- Description: Continuation is "same fg + underline as the preceding
  placeholder, no row/col diacritics → row unchanged, col+1", with run state
  reset on any non-placeholder. Kitty (and Ghostty runs) apply this only to the
  cell to the left on the same row. A new row with only a row diacritic (Kitty's
  2×3 example) must start at column 0, not `last_col+1` from the previous line.
  Bare cells must also inherit the MSB byte. The paint line `(r, c) = decode
  row/col diacritics, else continuation default` does not define "row present,
  col absent".
- Suggestion: Follow Kitty's three rules, left-to-right per row, resetting at
  each row start: no diacritics → inherit row, col+1, and MSB; only row, and
  previous cell has that row → inherit col+1 and MSB; row+col and previous col
  is one less → inherit MSB. Invalid diacritics count as absent (Ghostty). Test
  the first-column-only-row-diacritic case, not only a bare run on one row.
- Status: open

### Issue 5 — Severity: bug

- File: `2026-08-20-kitty-unicode-placement-design.md:142`
- Description: The new pass runs after text raster and never skips painting
  `U+10EEEE`. Host `blit_glyph_in` draws the first grapheme scalar when any font
  returns a non-zero glyph (`crates/prism-host/src/raster.rs`). On macOS that is
  the Last Resort tofu, tinted with the id-bearing fg — the screenshot this spec
  is meant to fix. `blit_rgba` / `blit_rgba_scaled` alpha-blend, so transparent
  mode-icons keep the tofu. Ghostty skips the placeholder as a font glyph and
  keeps the cell background (Kitty: bg shows through transparent images).
- Suggestion: In `rasterize_screen_at_with_theme`, if
  `cell.character == '\u{10EEEE}'`, fill the cell background and skip
  `blit_glyph_in` (and underline drawn from the id fg, unless a real underline
  is wanted). Then blit the tile. Unknown id: background only, no glyph.
- Status: open

### Issue 6 — Severity: bug

- File: `2026-08-20-kitty-unicode-placement-design.md:126`
- Description: Deletion says `d=A/a` clears both `images` and `registry`.
  Kitty: virtual placements are not on-screen, so `d=a/A/c/p/q/x/y/z` must not
  affect them. They are removed only by `d=i/I/r/R/n/N`. Clearing the registry
  on `d=a` would drop unicode images while placeholders remain, which is the
  opposite of the grid model.
- Suggestion: `d=a/A` continues to drop cursor `PlacedImage`s only. Delete
  registry rasters and virtual placements on `d=i/I` (and full
  `GraphicsState::clear()`). Do not treat `d=a` as a registry wipe.
- Status: open

### Issue 7 — Severity: bug

- File: `2026-08-20-kitty-unicode-placement-design.md:158`
- Description: Paint says `blit_rgba_scaled(sub-rect of img.rgba → this cell)`
  with no per-cell allocation. Current `blit_rgba_scaled`
  (`crates/prism-host/src/raster.rs`) takes a tightly packed `src_w×src_h`
  buffer with no source origin or row stride. A PNG row is `img.width` pixels;
  a tile is not a contiguous subslice. Passing `&img.rgba` with
  `src_w = width/cols` would read the wrong pixels. Copying each tile into a
  temp buffer breaks the fail-closed "no per-cell allocation" rule.
- Suggestion: Extend `blit_rgba_scaled` with `src_x`, `src_y`, `src_w`,
  `src_h`, and a source row stride (`img.width`), or add a sibling that samples
  that window. Reuse it for both direct and virtual blits.
- Status: open

### Issue 8 — Severity: bug

- File: `2026-08-20-kitty-unicode-placement-design.md:149`
- Description: `id = id_low | (msb_diacritic << 24)` uses
  `diacritic_index -> Option<u16>` (297 entries, indices 0–296).
  `256u32 << 24` overflows a `u32` and panics in debug. The spec requires
  malformed input never panics. Ghostty casts the third index to `u8` and
  treats overflow as absent.
- Suggestion: Treat a third-diacritic index `> 255` as missing MSB (0), or skip
  the cell. Compute `id_low | (u32::from(msb as u8) << 24)`.
- Status: open

### Issue 9 — Severity: suggestion

- File: `2026-08-20-kitty-unicode-placement-design.md:83`
- Description: SGR `58` lists only colon forms (`58:2::r:g:b`, `58:2:r:g:b`,
  `58:5:idx`). Prism already special-cases `38`/`48` semicolon groups so
  leftovers are not reused as SGR (`crates/prism-emulator/src/lib.rs`).
  A `58` arm that handles only in-group colons will ignore `58;5;n` /
  `58;2;r;g;b`, which some producers emit.
- Suggestion: Parse `58` with the same colon and semicolon walker as `38`/`48`,
  plus `59` reset. Consume semicolon arguments even for unknown submodes so they
  cannot apply as later SGR.
- Status: open

### Issue 10 — Severity: suggestion

- File: `2026-08-20-kitty-unicode-placement-design.md:135`
- Description: `image_id_low24` maps RGB and `Color::Indexed` only.
  `Color::Ansi` is a separate palette index (`SGR 30–37` / `90–97`). Ghostty's
  `colorToId` uses any palette index. `Color::Default → None` is correct.
- Suggestion: Map `Color::Ansi(n)` and `Color::Indexed(n)` to `u32::from(n)`.
  Keep `Default` as `None`.
- Status: open

### Issue 11 — Severity: suggestion

- File: `2026-08-20-kitty-unicode-placement-design.md:119`
- Description: Direct placement "pushes `PlacedImage` and upserts the registry"
  while `retained_bytes` "counts both". If each holds its own `Arc` clone of the
  same raster and both add `frame`, one image counts twice and the 16 MiB cap is
  effectively halved. Eviction "oldest-first" across two `Vec`s is unspecified
  (which collection is older?).
- Suggestion: Count unique rasters (Arc identity or an image-id entry). Evict
  from one oldest-first log that covers registry and cursor placements. Sharing
  the `Arc` is enough if the byte counter does not double-add.
- Status: open
