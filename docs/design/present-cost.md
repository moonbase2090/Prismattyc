# macOS present cost

Status: **proposal from a time-boxed spike (#199), not accepted direction.**
Nothing here changes shipped behavior. The [decisions](#decisions) need
MB2090's sign-off. This is the companion to the render-thread design in
PR #197 (`docs/design/render-thread.md`), whose decision F this spike pulls
forward.

Spike code (reference only, never merged): branch `spike/present-cost`,
commit `042adcf`, on `origin/main` `be4444c`. It builds on the
`spike/render-thread` harness. Every switch is an environment variable that
is off by default; see `spike/present-cost/README.md` on that branch.

## Summary

1. **CGImage tiles cost about 3.2 ms per Mpx for a full frame, at any size
   or scale.** That is 6.2 ms at 1.6 Mpx, 19.6 ms at 6.2 Mpx, and 46.7 ms at
   14.7 Mpx (5K, 2x). About 80% of it is `CATransaction` commit. #199's
   45 ms estimate for 5K was right.
2. **IOSurface-backed layer contents make the commit nearly free.** A
   commit that swaps an IOSurface takes 0.02–0.11 ms at every size. A full
   frame costs 5.3 ms at 6.2 Mpx and 6.5 ms at 5K. That cost is now the CPU
   copy and premultiply, not Core Animation.
3. **Present only what changed.** A two-row band costs 5.6 ms with tiles
   and 0.55 ms with an IOSurface written in place at 6.2 Mpx. One cell
   costs 0.94 ms and 0.08 ms.
4. **Most dirty tiles did not change.** In the 8-pane scenario, 104 of 112
   tiles were dirty at p50, but only 24 had different pixels. The Graphite
   border underlay alone adds 68 tiles per frame: it restores and re-strokes
   the ring around every pane on every partial frame. The damage was never
   short: 0 tiles changed outside it.
5. **In the host, IOSurface cuts present from 20.7 ms to 3.25 ms (p50)** in
   the 8-pane scenario, and from 20.9 ms to 3.0 ms under `yes`. Under `yes`,
   frames went from 24.8 to 45.5 fps; raster (15.7 ms) is now the largest
   cost.
6. **Output is pixel-identical.** All 120 probe cases read back equal to
   the premultiplied framebuffer. In the host, the IOSurface readback PNG
   and the `PRISMATTYC_DUMP_PRESENT` PNG for the same frame have the same
   SHA-256.
7. **Premultiply is most of the remaining cost.** For a 6.2 Mpx frame, a
   plain copy takes 0.33 ms. Copy plus today's per-pixel premultiply takes
   3.0 ms, even when every pixel is opaque. A branchless loop with the same
   arithmetic takes 2.1 ms, and skipping opaque chunks takes 0.9 ms. Both
   are bit-exact.
8. **CAMetalLayer is no faster for this workload and uses much more
   memory:** 5.5 ms full at 6.2 Mpx, plus 290 MiB footprint vs 88–136 MiB.

## Machine and method

| Item | Value |
| --- | --- |
| Source | `origin/main` `be4444c` plus spike commit `042adcf` |
| Machine | Apple M5 Max, 18 logical CPUs, 36 GiB, ARM64 |
| Display | Built-in Liquid Retina XDR, 3024×1964, scale 2 (the only display) |
| OS | macOS 27.0, build `26A428` |
| Rust | `rustc 1.98.1`, release profile |

**Probe** (`present_cost_probe`): one window per case. It never activates,
so it does not take focus. A straight-ARGB framebuffer changes each frame
in a `full`, `band` (full width, 82 px, two text rows), or `cell` (18×41)
region. Each case paints 150 frames at 60 Hz, 130 measured after warm-up,
on the main thread or a background thread. The probe reports CPU prep
(copy, premultiply, image or surface write), commit, and their total at
p50 and p95. It also records process CPU per frame, WindowServer CPU per
second (`ps` cumulative time, read-only), physical footprint, and GPU time
for Metal. After the run it presents one full frame and reads the presented
pixels back.

Backends:

- `tiles`: today's path. 512×128 tile sublayers. Each damaged tile is
  copied, premultiplied, wrapped in a `CGImage`, and set as contents.
- `ring`: one layer and three IOSurfaces. Each frame writes the damage, plus
  rects the chosen surface missed, into a surface that `IOSurfaceIsInUse`
  reports free, then swaps it in.
- `inplace`: one IOSurface written in place, then `setContentsChanged`.
  That selector is **undocumented**, and writing a surface that may be on
  screen can tear.
- `metal`: `CAMetalLayer`. Damaged rects are uploaded into a shared texture
  with `replaceRegion`, and a GPU blit copies it to the drawable, which is
  then presented.

Sizes: 1.6 Mpx and 6.2 Mpx at 1x and 2x, and 14.7 Mpx (5K) at 2x. Windows
are clamped to the 3024×1964 display, so the 6.2 Mpx and 5K layers extend
past the window. Upload and commit costs are real, but the compositor only
draws the visible part. "1x" is a layer with `contentsScale = 1` on this 2x
display.

**Host**: the isolated test instance from #197 (`env -i`, scratch `HOME`
and XDG, own pmuxd socket under `/private/tmp/pspk`). The live pmuxd and
the running Prismattyc were never touched. Scenario (c) has 8 pmuxd
sessions as 8 panes in one tab, a current Space, and 4 panes printing a
30-line listing every 200 ms. Scenario (b) is `yes` in one pane with the
#197 poll-drain fix, so frames paint. Each scenario was measured for 20 s
after warm-up. Each table is one run; raw output is in
`spike/present-cost/`.

**A measuring trap.** On the main thread, AppKit can already hold an
implicit `CATransaction`. An explicit `begin`/`commit` then nests inside it,
and the real work happens later, in the run-loop observer. In the probe, a
full-frame tile commit on main at 1.6 Mpx measured 47 µs without a flush
and 5.3 ms with `CATransaction::flush()`. The probe therefore always flushes. In the
host, `RedrawRequested` commits were not deferred: the tile commit measured
16.6 ms both with and without a flush. So #197's main-thread numbers stand,
but any new main-thread measurement must account for this.

## Measurements

### Full frame by size and scale

Main thread, total p50 in ms, with commit p50 in parentheses. Background
thread totals are in `matrix.txt`; see the main-vs-background table below.

| Size | Scale | Mpx | tiles | inplace | ring | metal |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1600×1000 | 1x | 1.60 | 6.4 (5.26) | 2.6 (0.06) | 1.8 (0.11) | 3.6 (0.17) |
| 3528×1764 | 1x | 6.22 | 19.5 (15.86) | 5.2 (0.03) | 4.8 (0.06) | 4.1 (0.05) |
| 1600×1000 | 2x | 1.60 | 6.2 (5.11) | 2.6 (0.07) | 1.9 (0.11) | 2.8 (0.14) |
| 3528×1764 | 2x | 6.22 | 19.6 (15.88) | 5.2 (0.04) | 5.3 (0.06) | 5.5 (0.05) |
| 5120×2880 | 2x | 14.75 | 46.7 (37.74) | 6.5 (0.02) | 6.5 (0.03) | 9.2 (0.04) |

Per Mpx (full frame, main, p50): tiles 3.1–4.0 ms; IOSurface 0.44–1.65 ms
(the per-Mpx cost falls with size because fixed costs amortize); Metal
0.63–2.27 ms. At equal pixel counts, 1x and 2x matched within 3% for tiles
and in place; ring and Metal differed by up to 10% and 34% in single runs.

### Partial frames

Main thread, total p50 in ms.

| Size | Scale | Damage | tiles | inplace | ring | metal |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| 1600×1000 | 2x | band | 3.18 | 0.15 | 0.65 | 0.45 |
| 3528×1764 | 2x | band | 5.56 | 0.55 | 1.10 | 0.67 |
| 5120×2880 | 2x | band | 6.38 | 0.74 | 2.46 | 1.25 |
| 1600×1000 | 2x | cell | 1.06 | 0.07 | 0.13 | 0.15 |
| 3528×1764 | 2x | cell | 0.94 | 0.08 | 0.14 | 0.17 |
| 5120×2880 | 2x | cell | 1.13 | 0.07 | 0.19 | 0.17 |

The 1x rows are close to the 2x rows at equal pixel counts (full table in
`matrix.txt`). The tile path pays 0.55–0.91 ms of commit for even one
tile. The ring writes each frame's damage into three surfaces in turn, so a
band costs 2–4× the in-place write. Metal pays about 0.04–0.17 ms of CPU for
`nextDrawable`, encode, and present, plus a GPU blit of the whole drawable
every frame: 0.1 ms at 1.6 Mpx, 0.6 ms at 6.2 Mpx, and 1.1 ms at 5K.

### Main thread vs background thread

6.2 Mpx at 2x, total p50 in ms, main / background:

| Backend | full | band | cell |
| --- | --- | --- | --- |
| tiles | 19.57 / 19.63 | 5.56 / 5.46 | 0.94 / 1.09 |
| inplace | 5.23 / 5.25 | 0.55 / 0.51 | 0.08 / 0.08 |
| ring | 5.26 / 5.23 | 1.10 / 1.42 | 0.14 / 0.15 |
| metal | 5.50 / 5.59 | 0.67 / 0.70 | 0.17 / 0.17 |

At 6.2 Mpx and 5K, full frames agreed within 2% on either thread. At other
sizes and for small partial frames, single runs differed by up to 36%
(full) and 55% (sub-millisecond partials) with no consistent direction,
so treat those differences as noise. Thread QoS made no difference: the
default and user-interactive QoS both gave 0.85 ms for a one-tile commit.

### CPU, GPU, and memory

Full frame, main thread. WindowServer CPU is noisy: it is shared with
everything else on the desktop.

| Size | Backend | Process CPU per frame | WindowServer ms/s | Footprint | GPU per frame |
| --- | --- | ---: | ---: | ---: | ---: |
| 6.2 Mpx | tiles | 19.8 ms | 88.8 | 140 MiB | — |
| 6.2 Mpx | inplace | 5.5 ms | 45.8 | 88 MiB | — |
| 6.2 Mpx | ring | 5.6 ms | 50.4 | 136 MiB | — |
| 6.2 Mpx | metal | 5.9 ms | 36.7 | 290 MiB | 0.34 ms |
| 14.7 Mpx | tiles | 47.3 ms | 74.6 | 302 MiB | — |
| 14.7 Mpx | inplace | 7.1 ms | 32.2 | 185 MiB | — |
| 14.7 Mpx | ring | 7.0 ms | 32.2 | 298 MiB | — |
| 14.7 Mpx | metal | 9.9 ms | 32.1 | 517 MiB | 0.88 ms |

The ring holds three frame-sized surfaces (about 25 MiB each at 6.2 Mpx),
but it drops the tile `CGImage`s, so its footprint is close to the tile
path's. Metal holds the canvas texture and up to three drawables.

### Where the full-frame write goes

`premultiply_bench`, 3528×1764, p50 over 60 iterations:

| Operation | Opaque input | 50% alpha input |
| --- | ---: | ---: |
| Copy only | 0.33 ms | 0.33 ms |
| Copy + `premultiply_in_place` (today) | 2.97 ms | 3.25 ms |
| Copy + skip 8-pixel chunks that are fully opaque | 0.90 ms | 3.48 ms |
| Copy + branchless `c * a / 255` (same arithmetic) | 2.08 ms | 2.08 ms |

The opaque-skip and branchless variants both assert bit-exact equality
with `premultiply_in_place`. Combining them would cost about 0.9 ms opaque
and 2.1 ms translucent; that is an estimate from the parts, not a
measurement.

### Why partial frames dirty almost every tile

Scenario (c), tile path, with the pixel oracle and per-stage counters on
(744 frames). "Added" is the number of new tiles a stage makes dirty.

| Stage | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| Compose (pane rows, scroll blits, focus, chrome boxes) | 24 | 112 | 112 |
| Activity headers, rail status, tab strip, sidebar | 0 | 0 | 0 |
| Pane bell restore | 0 | 0 | 0 |
| **Border underlay restore** | **68** | **92** | **92** |
| Final dirty tiles | 104 | 112 | 112 |
| Tiles whose pixels actually changed | **24** | 36 | 60 |
| Undamaged tiles that changed (missed damage) | 0 | 0 | 0 |
| Damage rect area, % of frame | 24% | 35% | 44% |

`BorderUnderlay` captures the 3 px ring band around every Graphite slot
before the rings are stroked. On each non-empty partial frame,
`restore()` writes those strips back and pushes every strip as damage, and
`paint_retained_graphite_panes` strokes every ring again. For a pane whose
ring did not change, the result is the same pixels, but its strips still
dirty every tile they cross. With 8 panes the strips cross almost every
512×128 tile. The damage *area* stays small (24%), so a present path that
writes exact rects is affected far less than one that replaces tiles.

Presenting only the truly changed tiles (`PRISMATTYC_SPIKE_SKIP_UNCHANGED`,
which costs a 0.76 ms diff) cut the tile path's present from 20.7 ms to
9.0 ms at p50. p95 stayed at 20 ms because full repaints remain full.

### In the host

| Scenario | Present path | Present p50 / p95 | Paint p50 / p95 | Frames (≈22 s) |
| --- | --- | --- | --- | ---: |
| (c) 8 panes | tiles | 20.7 / 24.9 ms | 27.2 / 45.9 ms | 411 |
| (c) 8 panes | tiles, only changed tiles | 9.0 / 20.0 ms | 13.9 / 44.8 ms | 493 |
| (c) 8 panes | IOSurface ring | 3.25 / 6.1 ms | 5.5 / 29.8 ms | 523 |
| (c) 8 panes, flushed | tiles | 20.7 / 24.3 ms | 26.5 / 45.9 ms | 453 |
| (c) 8 panes, flushed | IOSurface ring | 2.55 / 5.2 ms | 4.2 / 22.1 ms | 775 |
| (b) `yes`, flushed | tiles | 20.9 / 21.5 ms | 36.5 / 37.4 ms | 567 |
| (b) `yes`, flushed | IOSurface ring | 3.0 / 3.1 ms | 18.8 / 19.3 ms | 1,041 |

The IOSurface commit in the host was 0.04–0.05 ms p50. The ring wrote
12.4–19.9 MiB per frame at p50 in (c), 2–3× the damaged area: it
also writes the rects each surface missed and copies full frames after a
full repaint. Paint p95 stays near 22–30 ms because full-repaint frames
still raster the whole window (raster p95 18–25 ms). That is #197's
render-thread territory.

### Pixel identity

- Probe: all 120 cases read back `pixel-identical` (CGImage data for tiles,
  locked surface memory for IOSurface, canvas and drawable textures for
  Metal), including a translucent strip that exercises premultiply.
- Host: `PRISMATTYC_SPIKE_VERIFY` compared the shown surface with the
  premultiplied framebuffer every 30th frame: 28 checks, 0 mismatched
  pixels. The readback PNG and the `PRISMATTYC_DUMP_PRESENT` PNG for the
  same 3642×1716 frame have the same SHA-256 (`b5c3ea69…13e2b1`).
- Not verified: what reaches the glass. This terminal has no Screen
  Recording permission. Readback shows what Core Animation was given, not
  what the compositor drew.

## Recommended path

Present through **IOSurface-backed contents on one layer, written with
exact damage rects**, using a small ring of surfaces checked with
`IOSurfaceIsInUse`. Pair it with a faster bit-exact premultiply, and stop
the border underlay from re-dirtying unchanged rings.

Why this path:

- The commit stops scaling with pixels: 0.02–0.11 ms from 1.6 to 14.7 Mpx,
  vs 5–38 ms for tiles.
- The remaining cost is a CPU copy that partial damage shrinks directly
  (band 0.55–1.1 ms, cell under 0.2 ms at 6.2 Mpx).
- It uses documented APIs only (the ring, not `setContentsChanged`), keeps
  the CPU raster and `dump_present` unchanged, and keeps memory close to
  today's.
- It works the same on the main thread and a background thread, so #197's
  present thread can carry it later without redesign.

Not recommended now:

- **In-place single surface.** It is the cheapest for partial frames (no
  ring write amplification), but it relies on an undocumented selector and
  can tear. Keep it as a measured comparison only.
- **CAMetalLayer.** Its CPU cost is similar for full frames, it is slower
  at 5K (9.2 ms), and it adds 150–330 MiB, a GPU blit of the whole drawable
  every frame, and drawable pacing. Revisit if raster moves to the GPU.
- **Skipping unchanged tiles on the tile path.** It halves the p50 (20.7
  to 9.0 ms), but p95 stays at 20 ms, even a one-tile commit costs
  0.55–0.91 ms, and the diff adds 0.76 ms.

## Plan

Each PR is small, follows `REVIEW_POLICY.md`, and has a **Proof** section
with before and after tables from the isolated harness. Trunk behavior
ships behind a setting that is off by default; turning it on is its own PR.

| # | PR | Fits with | Proof |
| --- | --- | --- | --- |
| 1 | Add present sub-phases (write, commit, dirty and changed tiles, write bytes) to the #190 timing | #190; #197 plan PR 1 | `render-status --json` shows the new fields in scenario (c) |
| 2 | Bit-exact faster premultiply (skip opaque chunks, branchless loop) in `pixel_alpha` | new; also speeds the X11 and Wayland alpha paths | Exhaustive 2^32 or property test against today's function; bench table |
| 3 | IOSurface present backend behind `macos_present = "iosurface"` (default `tiles`): one layer, exact rects, three-surface ring, `wait_presented()`/readback hook for tests | new; replaces #197 plan PR 9 | Scenario (b) and (c) tables; readback equals `dump_present` (SHA-256); resize, scale change, and `macos_present_probe` |
| 4 | Ring write amplification: coalesce stale rects, and measure two vs three surfaces | new | Write bytes per frame and the busy-surface count before and after |
| 5 | Border underlay: restore and re-stroke only rings that change (focus, pulse, sweep, or damage under the ring) | new; answers #197 plan PR 9 | Damage stage counters: underlay tiles from 68 toward 0 at p50; oracle shows no missed damage |
| 6 | Turn on `iosurface` by default | flag flip (trunk) | Dogfood period; tables above |
| 7 | Rebase the #197 present thread (plan PR 10) on the IOSurface backend, if #190 data still shows present on main worth moving | #197 decision B, revisited | Same scenarios; main-thread paint p99 |
| 8 | Remove the tile path once IOSurface has shipped a release without regressions | new | Code removal; probe and tests updated |

PRs 2, 3, and 5 are independent and can run in parallel after PR 1. The
#191–#195 work is unaffected. #194 (fonts and glyph cache) and the #197
raster thread now matter more, because raster is the largest remaining
paint cost.

## Decisions

**A. Present through an IOSurface ring.** Recommended (see above).
Alternatives: in place (faster for partial frames, undocumented, may
tear), Metal (more memory, no CPU win), or skipping unchanged tiles (still
about 9 ms at p50).

**B. Use exact damage rects, not 512×128 tiles.** Recommended. Damage area
was 24% of the frame at p50 while tile coverage was 93%.

**C. Three surfaces.** Recommended for now: 0 busy-surface stalls were
measured. Two surfaces would cut write amplification; measure it in PR 4.

**D. Ship the faster premultiply before or with the IOSurface backend.**
Recommended. It is the largest remaining part of a full-frame present
(3.0 → 0.9 ms opaque at 6.2 Mpx), and it is shared with the other alpha
paths.

**E. Fix the border underlay damage separately from the backend.**
Recommended. With IOSurface it saves write bytes rather than whole tiles,
so it is lower priority than A–D but still removes about 68 dirty tiles
per frame on the tile path.

**F. Re-decide the present thread after IOSurface lands.** Recommended.
#197 moved a 21 ms present off main; with IOSurface, present on main is
2.5–3.3 ms at p50 in the host, and raster becomes the larger target.

**G. Defer Metal until raster moves to the GPU.** Recommended.
