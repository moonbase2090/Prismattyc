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
