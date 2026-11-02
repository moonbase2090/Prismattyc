# Native Lunatui backend: implementation plan

Status: plan, docs only. No code, no version bump, no behavior change. The
backend stays behind an off-by-default flag until its last slice lands.

## Goal

Let an application that runs inside Prismattyc render its cell grid straight
into the host, without ANSI, when the host and the app both support the
native grid protocol. Everywhere else the app keeps its ANSI output. The
acceptance target is that Scorecard runs natively inside Prismattyc.

## Contract

The wire contract is the lunatui proposal `docs/native-protocol.md` (lunatui
main, merged as `681ef9a`). This plan implements it on the host side and does
not change it. Any change to the contract goes back through the lunatui
protocol owner first.

The parts that matter here:

- Discovery uses the existing capability envelope at capability 0.4
  (`native.grid.v1`). Capability 0.1 to 0.3 replies stay byte-for-byte
  unchanged.
- One Unix socketpair per PTY generation. The host keeps one endpoint and
  passes exactly one endpoint into the child.
- Control records have a 32-byte little-endian header. Both peers check the
  effective UID before they accept a message or a descriptor.
- Frames live in a 4-slot shared-memory ring of 4,198,400 bytes. Each frame
  is a complete snapshot with a frame-local cluster table, and damage is only
  a hint.
- Any PTY resize revokes the native generation and returns the host to
  classic ANSI ownership.

## Current state

- The host advertises capability 0.1 to 0.3 only. No `native.grid.v1` key
  exists yet.
- The lunatui side is a proposal. The `lunatui-prismattyc` backend exists, but
  it has only been exercised as a grid sink.
- No shared-memory transport, control socket or native renderer exists in
  Prismattyc.

## Slices

Each slice is one PR, small enough to review, with its own tests. Every code
PR needs a SHIP from a reviewer in a different model family and a clean-VM
proof at the exact head.

1. **Host contract types (leaf).** Constants and encoders/decoders for the
   control header, the request and response bodies, and the frame header. Pure
   functions, no I/O. Golden byte tests for every message size in the table,
   and property tests for round trips. Rejects any length that is not exact.
2. **Capability 0.4 reply (trunk).** Adds the discovery reply behind the
   rich-feature flag, with `native.grid.v1` advertised only when the flag is on
   and the backend in slice 6 is available. Off by default. Tests show that the
   0.1 to 0.3 replies are byte-identical to today.
3. **Socketpair and peer credentials (trunk).** Creates the per-generation
   socketpair, passes one endpoint to the child, checks peer UID with
   `getpeereid` on macOS and `SO_PEERCRED` on Linux. Tests cover the wrong-UID
   and extra-descriptor cases.
4. **Shared-memory ring (trunk).** Creates and maps the anonymous 4-slot
   object, writes the header, transfers the single FD with `sendmsg`, and
   implements the producer and consumer sequence rules. Tests cover the
   sequence limits and the reject-on-consumer-greater-than-producer rule.
5. **Control state machine (trunk).** HELLO, SETUP, ACTIVATE, PING/PONG and
   RELEASE, with the 200 ms setup deadline, the 150 ms ping and 100 ms pong
   limits, and the lease rule. Any failure falls back to ANSI. Tests use a fake
   peer and a virtual clock.
6. **Native renderer (trunk).** Feeds the frames from the ring into Prismattyc's
   renderer. Off by default. The renderer reads the grid and damage, and the
   app never sees an ANSI difference in layout.
7. **Scorecard integration (acceptance).** Scorecard runs natively inside
   Prismattyc, with ANSI fallback verified outside it. This is the roadmap
   acceptance for PR 54.

## Flag and rollout

Rollout goes through a prerelease first:

- The first shipped build is a Prismattyc prerelease candidate, tagged
  `vX.Y.Z-rc.N` and marked as a prerelease. It never becomes the Latest
  release.
- `pmux update` skips prereleases unless the user passes `--pre`, so the RC
  reaches only people who ask for it.
- Promotion to a stable release happens only after the owner has tried the RC.
- Tags and releases are signed and cut by the owner. Feature PRs carry no
  version bump.

- One feature flag gates discovery and the renderer together. Default off.
- Turning the flag on by default is its own trunk PR with its own review.
- No version bump in feature PRs.

## Proof plan

- Every code slice: unit and property tests, plus the byte-level golden tests
  from slice 1.
- Slices 3 to 6: a clean-VM run at the exact head, with the control
  handshake exercised in a real child process.
- Slice 7: a Scorecard run inside Prismattyc with the flag on, and a run
  outside it with the flag off. Both must show the same layout.
- A resize test for every slice that touches ownership: the native generation
  must be revoked and the ANSI repaint must complete at the new size.

## Open questions

These come from the contract's own "before any host implementation" list and
must be answered before slice 3:

- The registry cut for capability 0.4.
- The inherited-FD convention and the PTY-generation offer binding.
- The real macOS and Linux credential semantics, checked on hardware.
- The no-truncation backing guarantee for the shared object.

## Out of scope

- Real alpha and sub-cell rendering in native mode (roadmap PR 55).
- The ratatui compatibility crate.
- Any change to the classic ANSI path.

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
