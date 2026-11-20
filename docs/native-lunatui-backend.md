# Native terminal sessions and images

Prismattyc accepts native cell grids and uploaded images from eligible direct
local PTY children when started with `--experimental-rich`. The feature is off
by default. Applications that do not negotiate a native session keep their
ANSI output.

This is the host implementation. The separately locked verification client
uses Lunatui's real grid, Image widget, discovery decoder and frame encoder.
A production Lunatui image backend and Scorecard integration remain separate
work. This feature does not change the published terminal compatibility claim.

## Supported sessions

| Host | Native grid minor | Capability tokens |
| --- | --- | --- |
| Supported 64-bit little-endian Linux with atomic 64-bit operations | 0 and 2 | `native.grid.v1`, `native.images.v1` |
| Native Apple Silicon macOS | 2 | `native.images.v1` |
| Windows, Intel macOS and other targets | None | None |

The flag gates discovery, transport and native presentation together.
Forwarded sessions and daemon-owned log views keep the Classic display source.
Linux serves minor 0 through its sealed exact-length backing. Darwin uses
rounded typed shared memory for minor 2 and does not advertise minor 0.
Minor 1 alpha attributes are not advertised by this implementation.

The authoritative layouts and negotiation rules are Lunatui's
[grid contract](https://github.com/moonbase2090/lunatui/blob/main/docs/native-protocol.md)
and [image contract](https://github.com/moonbase2090/lunatui/blob/main/docs/native-image-protocol.md).
Native images require `native.images.v1`. The grid token independently promises
live minor 0 support.

## Ownership and recovery

Each eligible PTY launch inherits one endpoint of a private Unix socketpair.
The host checks the peer identity, message bounds, descriptors, backing object
and generation before accepting a session. Every descriptor receive uses the
process launch boundary so a concurrent child cannot inherit received handles.

The producer writes complete snapshots into a four-slot shared-memory ring.
The host copies and validates a slot before it owns the scene. Text clusters,
wide cells, cursor and image placements are part of that immutable snapshot.
Damage rectangles are hints, not permission to omit validation.

A dedicated worker owns native transport, request ordering, resources and
leases. It answers control requests while the UI paints and consumes at most
one ring slot per turn into a single newest pending snapshot. Coalescing a
pending full snapshot does not install it. The UI installs that snapshot as
the common source for painting and readers, then acknowledges its session,
generation, metrics and frame identity before the worker sends the initial
ACTIVE reply. Revocation invalidates outstanding acknowledgements.

PTY output and geometry changes synchronously revoke ownership before the UI
parses bytes or changes metrics. A shared generation also invalidates a cached
scene after worker-side lease expiry. An unresponsive or failed worker closes
its endpoint and returns that pane to ANSI. A paint retaining an older scene
keeps its image references alive until that paint releases them.

An installed native scene supplies painting, selection, copy, find, links and
accessibility text. The ANSI emulator continues to retain its independent
content. A release, resize, metrics change, failed control exchange or expired
lease returns display ownership to that Classic source. Native content has no
scrollback. The retained Classic scrollback remains available.

A retained shell can launch a fresh native descendant after recovery. The new
session has a new generation, starts at frame 1 and uses the current grid and
physical cell metrics. Stale records cannot reactivate the old scene.

## Image limits and lifetime

Image upload uses file descriptors and explicit admission replies. Only READY
resources can appear in a presented frame. The host copies admitted source
bytes into owned storage and enforces these limits:

- 64 MiB per source.
- 128 MiB of admitted image storage per session.
- 256 MiB of admitted image storage per host process.
- 256 live resources per session.
- 256 placements per frame.

The renderer applies the source crop, destination geometry, opacity and depth
from the validated placement. A glyph above an image remains above it.
Omission removes a placement on the next presented snapshot. Retirement waits
until the resource is absent from installed scenes before storage is released.
A declined upload preserves the application's ANSI or cell fallback.

## Runtime verification

The repository's
[native verification map](../.cursor/skills/verify-prismattyc/features/native.md)
links the matching producer and isolated window drivers. Linux checks both
minor 0 and minor 2, actual X11 resize, image lifetime and ANSI recovery.
macOS checks both presenters, foreground suspension, quiet lease expiry,
live metrics changes, image limits and changed image generations.

Presented-frame captures record the application's presentation path. OS
screenshots are separate evidence for desktop composition. A complete feature
review also needs clean-VM proof at the reviewed commit and live text-reader
checks. Unit tests alone do not establish those results.

Negotiating Native ends the pane's existing Rich attachments and capability
grant. Release restores the retained ANSI grid and does not restore those
attachments. Applications that use both protocols must negotiate Rich again
after releasing Native ownership.

## Rollout

The feature remains experimental and off by default. Release-candidate use,
owner trial and performance comparison precede any default-on change.
Enabling it by default requires a separate trunk PR and owner approval.
Feature changes do not bump the workspace version or publish a release.

## No-regression gate

The native path has to be a measured improvement or equal everywhere. It is
never accepted on a regression. This section is defined before any code.

### 1. Baselines on current main

Record these on `origin/main` with the crossterm path, before the first code
PR, and keep the numbers in the plan PR:

- Render bench: `tests/native/render-bench.sh` through
  `tests/native/docker/run.sh render-bench`. The per-frame `cells_max`,
  `cells_mean` and `blit_sum` values for all eight experiments, with the Foot
  wall-clock reference at the same font size.
- Checkpoint latency: `tests/performance/checkpoint-latency.py` against a
  frozen release `pmuxd`, at 26 panes for 20 seconds, with and without
  persistence. Maximum pause, count above 50 ms, p50, p99.
- Output throughput: the Linux probe in `tests/performance`, with the frozen
  host binaries and Foot.
- Frame time p50 and p99, input-to-paint latency, CPU and resident memory
  for the same scenarios. The input-to-paint probe is the render bench plus a
  key-injected frame timestamp.

#### Recorded baselines (main `fc9b49d`)

Recorded on 2026-11-02 from `origin/main` at
`fc9b49d1bfcf6c84c92c4cdacadd91b7f04187ea`, crossterm path, before any
native code. Release binaries were cross-built with the release toolchain
(`cargo zigbuild --release --locked`, glibc 2.28) for both Linux targets.

| Binary | SHA-256 |
| --- | --- |
| aarch64 `pmuxd` | `04ec7ae9d05b58698f604455627e2653e7fc4234575377d792ec9d28e36f9f64` |
| aarch64 `prismattyc-host` | `13178a8a51a8c2770a6bc3af5eb59cec2e87c2177f8a60b53af91d2c4f16d702` |
| x86_64 `prismattyc-host` | `57edecdbc800ca00b68ffe3b2d7b2bc469d8bdaddb4edc7573810b83d4206fe3` |
| x86_64 `pmuxd` | `ef85ba21c2675c50d9a484b5ac1d6b92b56af341aaff771c751ec294fb032790` |

Environment: Docker on an Apple silicon Mac. Checkpoint latency and output
throughput ran natively in the `linux/arm64` performance image with
`--cpus=2 --memory=4g`. The render bench image is based on `archlinux:base`,
which has no arm64 build, so it ran as `linux/amd64` under emulation.
Emulation inflates absolute render times. Compare only runs from the same
environment.

**Checkpoint latency** (`checkpoint-latency.py`, 26 panes, 20 seconds):

| Persistence | Samples | p50 | p99 | Max | Over 50 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| on (11 checkpoints) | 3212 | 0.115 ms | 0.226 ms | 7.05 ms | 0 |
| off | 3312 | 0.120 ms | 0.245 ms | 1.29 ms | 0 |

**Output throughput** (`compare.py`, 3 trials, scale 20, Weston 13 GL, Foot
1.16.2). Main was run as both baseline and candidate (an A/A run), so the
spread column is this environment's noise. Values are medians of three.

| Metric | Main, run A | Main, run B | A/B spread | Foot |
| --- | ---: | ---: | ---: | ---: |
| Startup | 243.9 ms | 243.2 ms | 0.3% | 28.3 ms |
| ASCII | 56.95 MiB/s | 56.62 MiB/s | 0.6% | 149.04 MiB/s |
| Unicode | 44.27 MiB/s | 46.12 MiB/s | 4.2% | 92.71 MiB/s |
| Alternate screen | 52.93 MiB/s | 54.63 MiB/s | 3.2% | 208.26 MiB/s |
| Idle response p50 | 0.26 ms | 0.22 ms | 14% | 0.11 ms |
| Idle response p95 | 2.19 ms | 0.36 ms | 84% | 0.24 ms |
| Peak RSS | 149.3 MiB | 149.3 MiB | 0.0% | 27.2 MiB |
| CPU per trial | 2.28 s | 2.24 s | 1.8% | 0.56 s |

**Render bench** (`render-bench.sh`, 16 px font, x86_64 binaries under
emulation). Frame time is parse + damage + raster + present for each logged
frame. Run 1 is a second, otherwise identical run; notcurses was not
installed in run 1, so its numbers are not comparable.

| Experiment | Frames | `cells_max` | `cells_mean` | `blit_sum` | Frame p50 | Frame p99 | Run 1 p99 | Foot |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| single_key | 30 | 7106 | 7106 | 0 | 3.94 ms | 4.56 ms | 5.07 ms | |
| cursor_blink | 15 | 7106 | 7106 | 0 | 3.63 ms | 9.00 ms | 7.96 ms | |
| one_line_scroll | 48 | 7106 | 7106 | 0 | 3.64 ms | 5.66 ms | 5.10 ms | |
| pty_flood | 31 | 7106 | 7106 | 0 | 3.97 ms | 6.63 ms | 6.11 ms | |
| full_grid | 27 | 7106 | 7106 | 0 | 4.12 ms | 6.11 ms | 5.05 ms | |
| second_window | 5 | 7106 | 5684 | 0 | 5.33 ms | 5.51 ms | 5.83 ms | |
| vtebench_cursor_motion | 39 | 281 | 8 | 0 | 0.77 ms | 3.43 ms | 3.39 ms | 95 ms |
| vtebench_light_cells | 51 | 7106 | 140 | 0 | 0.74 ms | 3.06 ms | 2.61 ms | 89 ms |
| vtebench_dense_cells | 52 | 7106 | 137 | 0 | 0.73 ms | 2.78 ms | 3.57 ms | 92 ms |
| vtebench_scrolling | 48 | 7106 | 214 | 5994 | 0.76 ms | 28.48 ms | 35.35 ms | 94 ms |
| notcurses_demo | 696 | 7106 | 3826 | 1258 | 5.24 ms | 11.22 ms | | |

Every interactive experiment repaints the full grid (`cells_max` 7106,
`full_repaint_reason=fallback`) on this software-rendered X11 path.

**Not measured yet.** Input-to-paint latency has no probe; it needs the key
timestamp the plan describes and lands with the CI gate. Golden screen
captures are taken before the renderer slice, the first one that can change
output.

**Harness fixes made to record these numbers.** The render bench parsed
`render_timer` lines with a fixed field order, but the host now logs more
fields, so every experiment silently reported zero frames. It now reads fields
by name and also reports frame p50 and p99. Its Foot and notcurses install
failed in the Docker VM's seccomp sandbox and now uses
`pacman --disable-sandbox`, as the `xclip` install there already does. The
throughput probe required a 24-row pane, but Prismattyc's tab strip takes one
row of the same 80 × 24 window, so every trial failed. It now accepts 23 or
24 rows for Prismattyc, keeps Foot at 24, and stops if baseline and candidate
geometry differ.

**What this means for the gate.** In this environment the noise already
exceeds the 3% threshold: A/A throughput differs by up to 4.2% on Unicode
and 84% on idle-response p95, and render p99 moves 10% to 25% between runs of
15 to 50 frames. A 3% p99 gate would fail on noise alone. Before the gate is
enforced, it needs more frames and trials per scenario, native rather than
emulated hardware for the render bench, and a threshold set from measured
A/A spread. The CI gate change will propose those numbers.

### 2. CI gate

A CI job runs the same scenarios on the baseline and on the candidate, with
the native flag off and on. It fails when any of these exceeds noise:

- p99 frame time more than 3% worse than the baseline.
- Input-to-paint p99 more than 3% worse.
- Any correctness diff.
- Checkpoint maximum pause or the count above 50 ms worse than the baseline.

Each PR posts its before and after table in the PR body.

### 3. Golden output comparison

Every pmux screen gets a golden capture from real output, taken with the
crossterm path and again with the native path. Captures are compared pixel for
pixel where the renderer is equal, and reviewed by the owner where it is not.
No synthetic or re-rendered transcripts are used as goldens.

### 4. Flag policy

- The flag stays off by default until every scenario is equal or better and
  the owner has seen the comparison.
- Turning it on by default is its own trunk PR, with the comparison attached.
- A regression found after that point turns the flag back off in a hotfix PR.
