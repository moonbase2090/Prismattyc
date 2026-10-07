# Render thread and main-thread budget

Status: **proposal from a time-boxed spike, not accepted direction.** Nothing
here changes shipped behavior. The decisions in [Decisions](#decisions) need
MB2090's sign-off.

Spike code (reference only, never merged): branch `spike/render-thread`,
commit `6c36e8e`. Base: `origin/main` `f9c2d73`. All switches there are
environment variables that are off by default; see
`spike/render-thread/README.md` on that branch.

## Summary

1. **Heavy output freezes the macOS window today.** The `UserAction::Wake`
   handler pumps, and `pump()` re-sends `Wake` while output remains. winit
   0.30's macOS backend drains user events with `receiver.try_iter()` inside
   `cleared()`, so new `Wake`s keep the drain running and `RedrawRequested`
   and `AboutToWait` never run. Under `yes`, 20,166 pumps ran in 22.6 s with
   **0 paints**. Fixing only this collapses throughput (the 32 MiB flood went
   from 0.48 s to more than 40 s), so it must ship with a drain time budget.
2. **Present costs more than raster.** Core Animation present costs about
   21 ms of main-thread time per frame at 6.2 Mpx, and 17 ms of that is
   `CATransaction.commit`. Raster costs 3 ms for a partial frame and 16 ms
   for a full one.
3. **Core Animation can present from a background thread.** A probe
   committed 114 of 115 frames from a render thread while main slept for
   2 s. The commit costs the same on either thread (14.0 ms vs 14.2 ms), so
   moving it frees main without making frames cheaper. Bouncing images back
   to main for commit presented 0 frames while main was blocked.
4. **A present thread is a small change with a large effect.** In the host
   prototype, main-thread paint fell from 27.6 ms to 3.0 ms (p50) and from
   148 ms to 27 ms (max) in the 8-pane Space scenario. Under `yes`, frames
   doubled from 24.6 to 49.6 fps.
5. **Snapshot handoff is cheap. Untangling `HostState` is the real cost.**
   Copying a full 400×120 grid takes 22 µs. A damaged-row double buffer
   takes under 2 µs. `rasterize_frame` is a 1,673-line function that takes
   `&mut HostState`, reads 69 of its fields directly, and updates damage
   latches. Separating that is the real work of a raster thread.
6. **pmuxd and disk I/O on main cause the long stalls.** With the test pmuxd
   stopped, `refresh_space_views` calls blocked main for up to 2.0 s each,
   6.9 s of the 8 s in total. A pmuxd snapshot costs 3.4 ms to connect and register but
   only 0.11 ms to request. A once-a-second heartbeat `fsync` adds about
   4 ms.

## Machine and method

| Item | Value |
| --- | --- |
| Source | `origin/main` `f9c2d73` plus spike commit `6c36e8e` |
| Machine | Apple M5 Max, 18 logical CPUs, 36 GiB, ARM64 |
| Display | Built-in Liquid Retina XDR, 3024×1964, scale 2 |
| OS | macOS 27.0, build `26A428` |
| Rust | `rustc 1.98.1`, release profile, `--locked` |
| Font | Bundled JetBrains Mono Nerd Font, 30 px, cell 18×41 px |
| Present | Core Animation tiles (512×128 px), premultiplied ARGB |

The **test instance** was isolated with `env -i`, a scratch `HOME` and XDG
directories, and its own pmuxd socket under `/private/tmp/pspk`. The live
pmuxd and the running Prismattyc were never touched. Each scenario started
one host with `PRISMATTYC_SPIKE_CELLS=200x60` (the window opened partly off
screen at 6.2 Mpx). The runner discarded warm-up samples and measured for
20 s, or 40 s for the flood runs. Timing is wall clock around each phase on
the main thread (`spike_timing.rs`). Values are p50 / p95 / p99 / max in
microseconds unless marked ms. Each table is one run. Raw tables are in
`spike/render-thread/results/`.

Scenarios:

- **(a) idle**: one `zsh -f` pane, no input, test pmuxd running.
- **(b) heavy output**: one pane running `yes`.
- **(c) many panes + Space**: a current Space `spike` with 8 pmuxd sessions
  attached as 8 panes in one tab; 4 of them print a 30-line `ls` every
  200 ms.
- **(d) slow pmuxd**: scenario (c) with the **test** pmuxd (pid checked
  against the test socket) stopped with `SIGSTOP` for 8 s.
- **(e) flood**: one pane writes a fixed 32 MiB of 80-byte lines, and the
  child records the elapsed time.

## Measurements

### (a) Idle: main-thread cost of housekeeping

42 pumps in 21.3 s, all from `about_to_wait`, and no paints. Main was busy
for 235 ms (1.1%), but single pumps reached 18.5 ms.

| Phase | p50 | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| `pump()` total | 7,103 | 14,160 | 18,523 | 18,523 |
| `restart::poll` (component heartbeat, `sync_all`) | 4,259 | 7,283 | 10,473 | 10,473 |
| `refresh_space_views` | 613 | 5,710 | 7,273 | 7,273 |
| `local_views::persist_and_restore` | 501 | 919 | 963 | 963 |
| `publish_render_status` | 510 | 871 | 1,306 | 1,306 |
| `adopt_nested_attaches` | 94 | 308 | 324 | 324 |
| PTY drain | 12 | 22 | 24 | 24 |

`restart::poll` writes `host-<pid>.json` once a second through
`component_restart::atomic_json`, which calls `File::sync_all`. On macOS that
is `F_FULLFSYNC`.

### (b) Heavy output: the Wake starvation

| Build | Frames in window | Paint p50 | Notes |
| --- | ---: | ---: | --- |
| Shipped behavior | **0** in 22.6 s | — | 20,166 pumps, all from `Wake`; parse 1.10 ms each, 98.5% of wall time |
| Poll drain (`Wake` no longer pumps; leftover work uses `ControlFlow::Poll`) | 523 in 21.3 s (24.6 fps) | 37.7 ms | raster 16.4 ms (full: scroll overflow), present 21.3 ms (images 4.0, commit 17.3) |
| Poll drain + present thread | 1,131 in 22.8 s (49.6 fps) | 17.3 ms on main | raster 16.3 ms + tile copy 1.0 ms on main; 1,113 commits at 17.4 ms on the present thread |

Mechanism (winit 0.30.13, `platform_impl/macos`): `cleared()` delivers
`HandlePendingUserEvents`, then pending redraws, then `AboutToWait`. The user
event step is `for event in receiver.try_iter()` (`event_loop.rs:176`). The
`Wake` handler calls `pump()`, which re-sends `Wake` when `more` is true.
The PTY reader threads also re-send `Wake` once `pump()` clears
`wake_pending`. The iterator never finds the queue empty. A first spike that
only replaced the re-send with `ControlFlow::Poll` still painted once in
23 s for the second reason. The working spike makes the `Wake` handler a
no-op and pumps once per run-loop turn from `about_to_wait`. `wake_pending`
stays set until that pump, so the readers stop sending.

### (c) Many panes and a current Space

517 frames in 22.7 s (22.8 fps), 6.2 Mpx frames, 112 tiles.

| Phase | p50 | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| Paint total (main) | 27,569 | 41,018 | 47,159 | 148,473 |
| `rasterize_frame` | 2,955 | 20,185 | 26,171 | 51,810 |
| Present total | 20,917 | 23,913 | 25,912 | 96,641 |
| Present: tile copy + premultiply + `CGImage` | 4,095 | 5,120 | 5,735 | 20,776 |
| Present: `CATransaction` commit | 16,795 | 18,918 | 20,482 | 75,846 |
| Dirty tiles per frame (count, of 112) | 104 | 112 | 112 | 112 |
| `pump()` total | 9 | 272 | 6,386 | 12,096 |
| pmuxd connect + register (22 calls) | 3,355 | 6,548 | 7,381 | 7,381 |
| pmuxd `Snapshot` request on that connection | 108 | 186 | 285 | 285 |

448 of 517 frames took the partial raster path (`cells_painted` p50 279),
yet the present still dirtied 104 of 112 tiles at p50. The cause is not
established. One hypothesis to test is that the pane border or pulse strips
of 8 panes intersect almost every 512×128 tile.

With the **present thread** (same scenario, separate run):

| Phase (main thread) | p50 | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| Paint total | 2,990 | 15,633 | 26,834 | 27,446 |
| `rasterize_frame` | 1,649 | 14,459 | 25,650 | 26,282 |
| Present (tile copy into mailbox) | 1,142 | 1,511 | 1,668 | 2,224 |

Main painted 879 frames (38.7/s). The present thread committed 422 times
(18.6/s) at 16.9 ms p50. The difference was coalesced in the latest-wins
mailbox, not queued.

### (d) Slow pmuxd

Test pmuxd stopped for 8 s during scenario (c):

| Phase | p99 | max |
| --- | ---: | ---: |
| `refresh_space_views` | 5,108 | **2,002,150** |
| `pump()` total | 11,099 | **2,009,222** |
| `RedrawRequested` total | 46,787 | **2,057,296** |

`refresh_space_views` spent 6.9 s of the 8 s blocked in `live_snapshot()`
waiting on the 2 s socket timeout. The window did not paint or take input
during those waits.

### (e) Throughput under a 32 MiB flood

| Configuration | Elapsed | MiB/s | Frames in window |
| --- | ---: | ---: | ---: |
| Shipped behavior | 0.48 s | 67.3 | — (finished before the window opened; see (b) for frames) |
| Poll drain | not done after 40 s | < 0.8 | 731 |
| Poll drain + 8 ms drain budget | 1.61 s | 19.9 | 30 |
| Poll drain + 16 ms drain budget | 0.98 s | 32.8 | 15 |
| Poll drain + 8 ms budget + present thread | 0.99 s | 32.2 | 29 |
| Poll drain + 16 ms budget + present thread | 0.72 s | 44.7 | 15 |
| Poll drain + present thread, no budget | not done after 25 s | — | 1,219 |

Apart from the first row, the flood was the only output during the
window, so those frames were painted during the flood. Without a budget, each pump parses about 1.4 ms (8 chunks per
pane) between 20–40 ms paints.

### Snapshot handoff

`snapshot_handoff_bench` (`prismattyc-core` example), real
`prismattyc_core::Cell` (36 bytes), 2,000 frames per row, consumer on a
second thread. Producer-side cost in µs, p50 / p95, from one run; two
further runs agree within 10% at p50.

| Strategy | Damaged rows | 200×60 (421 KiB) | 400×120 (1,687 KiB) |
| --- | --- | ---: | ---: |
| Full copy into a new `Vec` | all | 5.7 / 6.5 | 22.4 / 24.8 |
| Full copy into a recycled buffer | all | 5.0 / 6.2 | 23.0 / 24.7 |
| Double buffer, copy damaged rows, swap under mutex | 1 | 0.5 / 0.6 | 0.6 / 0.8 |
| Double buffer | 5 | 1.1 / 1.3 | 1.9 / 2.2 |
| Double buffer | all | 10.5 / 12.3 | 42.9 / 48.6 |
| Per-row `Arc<[Cell]>`, publish `Arc<Frame>` | 1 | 1.3 / 2.9 | 3.7 / 5.4 |
| Per-row `Arc` | 5 | 2.2 / 3.9 | 5.2 / 6.7 |
| Per-row `Arc` | all | 12.9 / 13.7 | 41.9 / 43.2 |
| Thread wake after channel send (consumer parked) | — | 1.9 / 4.7 | — |

The double buffer refreshes this frame's and last frame's damaged rows, so
its full-damage cost is two grid copies. `Cell.cluster` is a handle into the
owning screen's cluster table, so a snapshot must carry that table (or
resolved graphemes) for every referenced cluster. The bench does not include
that cost.

## Where frames can present from

| Backend | Present off main? | Evidence |
| --- | --- | --- |
| macOS Core Animation tiles (`MacPresent`) | **Yes**, for our standalone tile `CALayer`s inside an explicit `CATransaction` | Measured, see below |
| softbuffer (X11; macOS fallback) | Allowed by its types: `Surface` is `Send` since softbuffer 0.4.4, and creation of the CG backend checks for the main thread | Source reading of softbuffer 0.4.8; not run |
| Wayland `wl_shm` (`WaylandShm`) | Likely: `wayland-client` `Connection` is thread-safe, but the present thread must own the `EventQueue` that receives buffer releases | Source reading; not run (no Linux machine in this spike) |
| wgpu (`--features gpu`, 26.0.1) | Device, queue, and surface are `Send + Sync` on native targets. Create the surface on main | API bounds only; not built or run |

### macOS evidence

`render_thread_present_probe` creates a 3024×1716 px window and 84 tile
sublayers on main. A render thread then fills 240 frames at 60 Hz, builds
the tile `CGImage`s, and commits. Main sleeps for 2 s from frame 30, as a
slow pump would. Two runs per mode gave the same commit counts.

| Mode | Image build p50 | Commit p50 / p95 / max | Commits while main slept |
| --- | ---: | ---: | ---: |
| `bg`: render thread commits | 3.1 ms | 14.0 / 14.5 / 16.0 ms | **114 of 115** frames |
| `main`: images sent to main, main commits | 4.0 ms | 14.2 / 15.1 / 31.1 ms | **0** (119 frames waited) |

Caveats:

- A background commit completing is strong evidence that the transaction
  reached the render server. It is not a screenshot. This terminal lacks
  Screen Recording permission, so the probe's two capture points failed. A
  reviewer with that permission can run the probe with `PROBE_CAPTURE_DIR`
  set and compare the two PNGs.
- The `CATransaction` documentation says implicit transactions commit when
  the thread's run loop next iterates. A render thread has no run loop, so
  it must use explicit `begin`/`commit`, as the probe and prototype do.
- `NSView`, its backing layer, and window chrome stay on main. The prototype
  also keeps tile geometry (rebuild, resize, scale) on main and first waits
  for the present thread to go idle.
- Commit cost does not depend on the thread. It is about 2.7–3.2 ms per
  Mpx for full-frame tile replacement. Making it cheaper (for example
  IOSurface-backed contents updated in place, or `CAMetalLayer`) is a
  separate, unmeasured question.
- Commits are not paced to the display. The prototype commits as soon as a
  mailbox has tiles, and the latest-wins mailbox bounds the backlog to one
  frame.

## Proposed design

### Threads

| Thread | Owns | Stage |
| --- | --- | --- |
| Main (winit) | Event loop, input, IME, a11y, `HostState`, VT parse, pane and layout model | today |
| PTY readers, log readers, config watcher, git info | (existing) | today |
| pmuxd client | One long-lived control connection, latest `Snapshot`, Space status, autosave | #192, #191 |
| File writer | Latest-wins atomic writes: render status, attach-tabs cache, component heartbeat | #195 |
| Present (one per window) | Tile images and `CATransaction` commit (macOS); buffer present on other backends | new |
| Render (one per window) | Raster from a frame snapshot, glyph cache, fonts | new, after #194 |

### Stage 1: present thread (prototype exists)

Main still rasterizes into the retained straight-ARGB framebuffer. It copies
the damaged tiles (1.1 ms p50 in scenario (c)) into a mailbox and returns.
The mailbox is a `BTreeMap<tile index, pixels>` behind a mutex and condvar.
A newer frame replaces an unsent tile, so a slow commit coalesces frames
instead of queueing them. The present thread premultiplies, builds
`CGImage`s, and commits. A rebuild or scale change waits for idle, applies
geometry on main, and hands the thread the new layer set.

Gaps before it can ship:

- Tests and `PRISMATTYC_DUMP_PRESENT` need a `wait_presented()` hook. The
  existing `macos_present_probe` passes its geometry checks on the threaded
  path, but its synchronous `contents` check fails without the hook (as
  expected).
- Present errors become asynchronous, so `present_succeeded` and
  `pending_full_repaint` need a completion message from the thread.
- Recycle tile buffers instead of allocating them per frame.
- `Send`/`Sync` are asserted by hand for `Retained<CALayer>`. This needs a
  focused `unsafe` review.

### Stage 2: render thread

Main publishes an immutable `FrameSnapshot` per window. The render thread
owns the framebuffer, `FontMetrics`, and the glyph cache from #194, and it
feeds the present thread.

```text
FrameSnapshot {
    generation, size_px, scale,
    panes: Vec<PaneSnapshot {           // damaged-row double buffer per pane
        rect, rows, damage, cursor, selection, scroll, cluster_table,
    }>,
    chrome: ChromeSnapshot,              // already computed: frame_damage_snapshot()
    overlays: OverlaySnapshot,           // palette, toasts, prompts, splash, find, OSD
    theme: Arc<Theme>, opacity, background: Arc<BackgroundLayer>,
}
```

The order is refactor first, then move. First extract
`compose(&FrameSnapshot, &mut RasterState, &mut [u32]) -> FrameDamage` and
call it on main, with pixel-identical `dump_present` output as the proof.
Then move the call to the render thread behind a flag. The coupling to cut:
`rasterize_frame` reads 69 distinct `HostState` fields directly (mux,
chrome, overlays, hover, animation) and calls dozens of helpers that take
the whole host. It also writes state that later frames read
(`pane_damage`, `last_pane_views`, `last_chrome_snapshot`, `render_frame`,
pane bell settling). Those latches move to render-thread state, and pane
damage moves into the snapshot.

### Risks

- **Throughput regression.** Fixing the starvation alone takes the flood
  from 0.48 s to more than 40 s. The fix and the drain budget must land in
  the same PR.
- **Two writers to the layer tree.** Resize while a commit is in flight. The
  prototype waits for idle; that adds resize latency up to one commit
  (17 ms).
- **Snapshot completeness.** Clusters, hyperlinks, images (graphics),
  selection, and hover all need to enter the snapshot, or the render thread
  shows stale or wrong glyphs.
- **Font and glyph ownership.** Fonts load on main today (#194). The render
  thread must own its caches, and font swaps become messages.
- **Pacing.** Without display-link pacing, the present thread commits as
  fast as frames arrive. Coalescing bounds the backlog but not wasted work.
- **Linux untested.** Nothing in this spike ran on X11 or Wayland.

## Plan

Each PR is small, follows `REVIEW_POLICY.md`, and has a **Proof** section.
Trunk PRs that add behavior are flag-gated and off by default; turning a
flag on is its own PR. "Scenario" refers to the isolated harness above.

| # | PR | Issue | Proof |
| --- | --- | --- | --- |
| 1 | Per-phase pump timing in `render_timer` and `render-status` (the spike's phase list) | #190 | Scenario (a) and (c) tables from `render-status --json`; frame time unchanged with timing off |
| 2 | macOS Wake starvation fix (pump once per turn from `about_to_wait`) **plus** drain time budget (default per decision A) | new + #193 (1) | Scenario (b) paints > 0; flood table; a new regression test (filed with the new starvation issue) that sustained `Wake`s still reach `RedrawRequested`; existing child-EOF wake and exit-cascade tests pass |
| 3 | Long-lived pmuxd snapshot client and cache; migrate periodic callers | #192 (1) | Scenario (d): no pump over the budget while pmuxd is stopped; 0 connects per second in steady state |
| 4 | Space poll and autosave on the client thread; use `file_config` instead of `config::load` | #191 | Scenario (c): Space phases under 1 ms at p99; autosave state tests |
| 5 | File-writer thread for render status, attach-tabs cache, and the component heartbeat (drop the heartbeat `fsync` or move it off main) | #195 (3) + new | Scenario (a): `restart::poll` and `publish_render_status` under 0.1 ms |
| 6 | Paste through the writer thread; image paste encode off main | #195 (1, 2) | 5 MB paste into a slow reader: no main-thread sleep, frames continue |
| 7 | Async pane connect and promote | #192 (2) | Opening 8 pmuxd panes: no pump over budget |
| 8 | Log batch splitting; off-thread restore decode | #193 (2, 3) | Large-scrollback Space reopen: pump within budget |
| 9 | Find why partial frames dirty 104 of 112 tiles; fix the damage-to-tile mapping if the cause is confirmed | new | Scenario (c): dirty tiles and commit time before and after |
| 10 | macOS present thread behind `present_thread = true` (off by default), with `wait_presented()` for tests | new | Scenario (b) and (c) tables; present probe; resize and scale test |
| 11 | Glyph cache and fonts off main | #194 | Per #194, plus raster p95 in scenario (c) |
| 12 | Extract `FrameSnapshot` and `compose()`, still on main (pure refactor) | new | Pixel-identical `dump_present` across the existing render-window tests |
| 13 | Raster on a render thread behind a flag | new | Scenario (b), (c), and (e): main-thread paint p99, fps, and flood MiB/s |
| 14 | Present thread for softbuffer and Wayland `wl_shm` after a Linux measurement pass | new | Same scenarios on a Linux host |
| 15+ | Flag flips (each its own trunk PR) | — | Dogfood period plus the tables above |

PRs 3–8 are independent of each other after PR 1 and can run in parallel.
PR 10 does not depend on PRs 3–9.

## Decisions

**A. Ship the starvation fix and the drain budget together, with an 8 ms
default budget.** Recommended. Measured: 8 ms gives 19.9 MiB/s at about
20 fps; 16 ms gives 32.8 MiB/s at a 56 ms frame. With the present thread,
8 ms reaches 32.2 MiB/s. Alternative: 16 ms for throughput, at the cost of
input latency.

**B. Build the present thread before the render thread.** Recommended.
Present is the largest measured main-thread cost (21 ms p50 in (c)). The
prototype is about 250 lines and needs no `HostState` refactor.

**C. Snapshot strategy: damaged-row double buffer per pane.** Recommended.
1, 5, or all damaged rows cost 0.6, 1.9, or 43 µs at 400×120. Per-row `Arc`
costs more for typical damage and adds allocation. Full copies are simpler
but cost 22 µs at 400×120 every frame.

**D. One present thread per window.** Recommended for simplicity: one
mailbox, no cross-window ordering. Alternative: one shared thread that
serializes windows.

**E. Frame pacing: start unpaced with latest-wins coalescing.** Recommended.
Add display-link pacing only if #190 data shows wasted commits. Alternative:
pace from `CADisplayLink`, which needs a main-run-loop or `CVDisplayLink`
callback thread.

**F. Spike the commit cost separately (IOSurface or `CAMetalLayer`).**
Recommended after PR 10. The commit is 17 ms at 6.2 Mpx on any thread.

**G. Defer Linux present threads until measured on Linux.** Recommended.
This spike has no Linux numbers.

**H. Treat the starvation fix as a pure fix, so it needs no feature flag.**
Recommended. It restores the intended "one drain pass per event-loop
cycle, then yield" behavior described in the comment at
`crates/prismattyc-host/src/main.rs:3225` (that comment's "#183" is an
older tracker number, not GitHub #183), and it is measurable. The
drain budget value is the only tunable.

## Reusable findings for lunatui

These are facts and numbers from this spike. lunatui owns its own decisions.

**Handoff pattern.** With 36-byte cells, a full grid copy costs 5.7 µs at
200×60 and 22 µs at 400×120. A damaged-row double buffer costs 0.5–1.9 µs
for 1–5 rows, and twice a full copy when every row is damaged (it refreshes
this frame's and last frame's rows). Per-row `Arc` publishing costs
1.3–5.3 µs for 1–5 rows and gives no advantage at full damage. Waking a
parked consumer costs 1.9 µs p50 and 4.7 µs p95. Handoff was never more
than 0.2% of a frame here. lunatui's 16-byte cell would scale copy costs
down roughly with size; that is an estimate, not a measurement.

**Render vs encode-and-write split.** Moving the expensive output step
(here, `CATransaction.commit`) to its own thread freed the main thread but
did not make the step cheaper: 14.0 ms vs 14.2 ms. In a TUI the analog is
the writer thread: `write()` time to a slow terminal stays the same, but
input handling stops waiting on it. The handoff that worked was "producer
copies damaged regions into a mailbox and returns".

**Back-pressure.** A latest-wins mailbox keyed by region (tile index here,
row index in a grid) merged a 38.7 fps producer onto an 18.6 fps consumer
without queueing, and the main-thread frame cost stayed flat. Bounding by
region, not by frame count, keeps memory at one frame.

**Event-loop wake lesson.** A "more work" signal must not re-enter the queue
it is being drained from. If the drain loop keeps taking new items while
producers or the handler keep sending, rendering never runs. Here that meant
0 paints in 22 s. The fix that worked was a coalescing flag that is cleared
only by the consumer's work step, with one work step per loop turn.

**Pacing vs throughput.** Without a time budget, parse got 1.4 ms per
40 ms frame and throughput fell from 67 MiB/s to under 0.77 MiB/s. An 8 ms
budget gave 19.9 MiB/s (32.2 MiB/s once output moved off the thread); 16 ms
gave 32.8 (44.7). Parse throughput and frame rate trade directly unless
output runs on its own thread.

**Shared crate or Prismattyc-specific.**

| Piece | Size in spike | Prismattyc-specific? |
| --- | --- | --- |
| Latest-wins region mailbox (mutex + condvar, merge on submit) | ~60 lines | No; generic over region key and payload |
| Per-phase timer with percentile report and reset file | ~150 lines with the spike switches | No |
| Coalesced wake flag pattern | a few lines | No (the winit detail is Prismattyc-specific) |
| Core Animation tile present, `CALayer` thread rules | ~150 lines | Yes |
| winit macOS user-event drain behavior | — | Yes (Prismattyc uses winit; lunatui does not) |
| `FrameSnapshot` contents | — | Yes (host chrome, overlays, Spaces) |
