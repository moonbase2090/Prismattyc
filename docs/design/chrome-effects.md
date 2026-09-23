# Event-driven host chrome

Status: PT-308 design proposal. The [interactive preview](../../demo/chrome-effects/index.html)
illustrates behavior; it is not the native presenter or performance evidence.

## Decision for review

Compare two original designs using the preview:

1. **Quiet signals (recommended starting point):** focus uses the existing static
   border. A bell adds a small pane-local badge until the user visits that pane;
   a bell in the already focused pane clears on the next interaction there.
   No decorative timer is needed. Space selection uses the existing selected chip.
2. **Timed accents:** focus/Space entry adds a 120 ms border accent; a bell adds
   a 360 ms amber border and badge on its owning pane. The accent is a static hold
   followed by cleanup, without fade frames. Repeated events during a hold coalesce
   without extending its deadline. Reduced motion uses quiet signals.

These timings are design candidates. Both modes are optional and off by default.
Existing border animation remains a separate, already configurable behavior.
Do not enable both focus cues for the same event. Existing audible notifications,
attention state and legacy visual-bell configuration retain their meaning; choosing
an optional pane-local bell must explicitly replace, not stack with, the full-frame
flash for that event. Configuration names and migration are not yet selected.

## Current host seams

Audited at `9b7f6b26060bd19598765f17cc156238b951821e`:

- `main.rs::next_control_flow` merges border/pulse and cleanup deadlines into
  `WaitUntil`; absent deadlines use `Wait`. The host also has a separate one-second
  cache heartbeat. Zero *additional decorative* wakeups does not mean a globally
  idle event loop with no existing deadlines.
- `rasterize_frame` detects pane focus transitions and starts the existing optional
  border sweep. `BorderUnderlay` restores its bounded strips before content paint.
- `drain_pty` receives pane IDs from `take_pending_bells`; its existing visual bell
  stores a window-level `bell_flash`. Toasts already have pane owners and expiry.
- `frame_chrome_snapshot` records paint-visible chrome at one frame timestamp.
  Its visibility rules must continue matching the painter.
- `pane_screen_paint` and `pane_surface_alpha` implement existing inactive-pane
  background weighting/alpha. Preserve foreground text, explicit cell backgrounds,
  image and transparency semantics; do not multiply the whole pane by a new mask.

Source paths above are in `crates/prismattyc-host/src/`. No native behavior changes
are included with this proposal.

## Lifecycle contract

Owner identity is `(window, stable Space ID, pane ID, view generation, effect kind)`.
Names and current focus are not sufficient owners. Each active record stores its
paint footprint, cleanup footprint, deadline if any, and last presented generation.
One record per owner/kind; state is bounded by live views, never event volume.

| Event | State change | Damage / scheduling |
| --- | --- | --- |
| Focus moves | Clear acknowledged badge; replace prior focus accent | Union of old/new accent footprints; ordinary focus/background damage remains valid |
| Space switches | Cancel departing view effects; cue entered view only | Existing layout/content transition plus entered footprint; no new whole-grid decorative pass |
| Bell | Set badge or timed accent for the ringing owner | That owner's visible footprint only; hidden view updates attention metadata |
| Duplicate bell during hold | Coalesce | No deadline extension or additional scheduled frame |
| Timed hold ends | Remove effect, retain one cleanup request | One cleanup repaint, then no effect deadline |
| View hidden/occluded | Cancel transient presentation and timer | Remember dirty cleanup for next visible paint; no hidden presents |
| Window loses focus | Cancel focus accent; visible bell indication remains pane-local | No continuous decorative pulse; quiet badge persists |
| Pane/view removed | Remove record and deadline | Restore old footprint only if a surviving surface needs it |
| Resize/DPI/theme/full paint | Invalidate retained pixel storage | Normal rebuild regenerates overlay state against current geometry |
| Reduced motion | Convert held bell to quiet badge; clear timed focus | One cleanup; no frame-by-frame animation |
| Effects disabled | Clear records and retained storage | One cleanup if pixels were presented; no subsequent effect scheduling |
| Cursor hidden | Stop cursor-specific effects only | Bell and attention handling remain independent |

Cleanup is idempotent. A deadline remains represented until cleanup has actually
been submitted. Failed presentation must preserve its damage for retry. A new view
with a reused pane ID cannot inherit old overlay pixels. Returning to an occluded
window must not replay an expired focus flash or a backlog of bell pulses.
A quiet bell indication may persist through occlusion until acknowledged.

## Geometry and ownership

Use a clipped two-pixel perimeter for candidate border accents and a measured
badge rectangle inside pane chrome. Never draw outside that view's surface.
Small panes fall back to the available outline; omit an unreadable badge.
Every damage rectangle includes the actual stroke/antialias footprint. Tile-copy
measurements use whole intersecting tiles, not the ideal rectangle area.

Capturing/restoring a border cache does not by itself clean a badge outside those
strips. Either save the badge underlay or use the existing correct row/pane repaint
fallback for that cleanup. Prevent framebuffer scroll copies from sampling chrome.
Simultaneous bell and focus share the same perimeter: bell wins temporarily, then
restore the current focus appearance. Preserve alpha, edge backgrounds and split
geometry. Static inactive-background treatment changes only on actual focus/content
or configuration events and adds no timer.

## Backend choice

Start with existing bounded software paint and damage submission. It already has
cross-backend cleanup and scroll protection from PT-307. Add no new GPU pass by
assumption. A separately retained overlay layer is a comparison candidate only:
it might avoid tile copies, but adds layer/compositing cost, geometry synchronization,
alpha behavior and lifetime management. Choose it only with measured end-to-end
benefit on the native backend. Do not infer native cost from the DOM preview.

## Performance acceptance

No measurable regression beyond established repeat-run noise is allowed for either
disabled or enabled effects. If a cue regresses performance, simplify or omit it.

Use frozen baseline/candidate binaries with identical config, fonts and workload;
interleave repeated trials. Cover idle, occluded, sustained ASCII/Unicode output,
multi-pane output, rapid focus/Space changes, bell storms, scroll and resize.
Record throughput, input-to-display latency, frame-time tails, CPU, memory, rasterized
cells/pixels, copied tiles/pixels, wakeups and presents. Report uncertainty and native
platform coverage separately. A zero-cell frame is not proof of zero copy/GPU cost.

Required behavioral evidence before native acceptance:

- Pixel equality against full repaint during and after every lifecycle transition.
- Unrelated panes receive no bell-only content repaint.
- Disabled/inactive effects add zero timer wakeups and periodic presents.
- Bell storms keep bounded state and deadlines; stalled presentation does not lose cleanup.
- Static dimming preserves explicit backgrounds, readable foregrounds and alpha.
- Native macOS, Linux and Windows results remain distinct.

Daytime mutations select changed lines only. Reuse valid completed mutation results;
queue expensive full-suite fallback for the nightshift runner.

## Provenance

This proposal and preview were written independently from the existing Prismattyc
host implementation and user requirements. No external implementation, shader,
asset, test or documentation text was imported. The preview uses original HTML/CSS
and event handling. This records the implementation process, not legal clearance.
