# Pane-local visual bell

PT-308 scope narrowed with user approval: a pane-local visual bell and concrete
bell cleanup fixes. Existing focus borders, optional light-cycle animation and
inactive-pane background treatment keep their current behavior. No shared effects
framework, additional focus/Space cue, shader interface or GPU pass is introduced.

## Configuration and behavior

`pane_visual_bell = true` opts into a static, two-pixel inverted pane perimeter.
It requires `visual_bell = true`. The new option defaults to false. The perimeter
holds for 120 ms with a start and cleanup paint; no fade or animation ticks.
Repeated bells during the hold do not extend its deadline or queue pulses.

For a visible pane, this replaces the full-window flash and the BEL toast for that
event. Audible bells, OS notifications, attention handling, write-failure chips
and other toasts retain their existing behavior. Hidden panes retain existing toast
and attention handling without replaying a perimeter accent when revealed.
Setting `visual_bell = false` disables either visual flash mode; the existing toast
setting remains independent in that case. No automatic reduced-motion preference
integration is added; the accent has no movement and can be disabled explicitly.

The [browser preview](../../demo/chrome-effects/index.html) illustrates the narrowed
interaction only. It is not a native rendering or performance measurement.

## Ownership and cleanup

Each live record owns a Space ID, pane ID, runtime identity, geometry and deadline.
The existing per-runtime identity is minted even for plain terminals. Replacing a
runtime in the same pane slot cannot inherit a previous bell. State is bounded by
visible panes plus captured cleanup from the last painted frame, not event volume.

| Event | Behavior |
| --- | --- |
| Visible bell | Start one hold for the ringing pane; coalesce duplicates |
| Deadline | Cancel the hold and request cleanup |
| Window occluded | Cancel holds and their deadlines; retain captured cleanup |
| Space/view switch, pane removal or replacement | Cancel accents whose owner is no longer visible; parked-view swaps explicitly cancel |
| Geometry change | Cancel affected accents; the normal layout repaint handles the new geometry |
| Configuration disables or changes mode | Clear the old visual cue; clearing a full-window inversion requests a full repaint |
| Window loses focus | A visible pane's bell keeps its fixed deadline |
| Cursor hidden | No effect on bells |
| Failed present | Existing host fallback retries a full repaint |

Expired/cancelled records retain captured pixels until raster cleanup. Records
that never captured pixels can be removed immediately. There is no deadline after
cancellation. The host's existing cache heartbeat and other notifications remain
independent; zero additional bell deadlines is not a claim of globally zero wakeups.

## Rendering

Reuse `BorderUnderlay` and the existing software damage/presentation path. Capture
only border strips, invert only the two-pixel perimeter, preserve pixel alpha, and
restore bell pixels before focus-border restoration and content/scroll painting.
This ordering prevents scroll copies from sampling the bell. The saved strips use
the existing seven-pixel border budget; this is intentionally larger than the
painted perimeter and must be counted in performance measurements.

Bell start and cleanup explicitly contribute strip damage without changing the
whole-window bell guard or the general chrome signature. Unrelated content still
uses its own existing damage decisions. Other active overlays may independently
require a full repaint. No extra badge or content-row repaint is introduced for
the new visible-pane cue. Startup prompts and splash remain above the accent.

## Acceptance before shipping

No measurable regression beyond repeat-run noise is acceptable with the option
both disabled and enabled. Compare frozen baseline/candidate binaries with the
same config, fonts and workloads. Include single/multiple panes, idle/occlusion,
ASCII/Unicode output, simultaneous and repeated bells, focus/Space changes, scroll,
resize, pane replacement and mode changes during a live flash.

Record throughput, input latency, frame-time tails, CPU, memory, rasterized cells,
copied pixels/tiles, presents and timer wakeups. Check pixel equality against full
repaint during and after cleanup, including overlapping focus animation and failed
presentation. Report native Linux, macOS and Windows evidence separately. A
zero-cell frame does not establish zero copy or GPU cost. Reject or simplify the
cue if enabled-path performance regresses.

Implementation and compile evidence do not satisfy these behavioral/performance
gates. Daytime mutations cover changed lines only; completed mutation results are
reused while their relevant inputs remain unchanged.

## Provenance

Written independently using existing Prismattyc code and user requirements. No
external implementation, shaders, assets, tests or documentation prose were
imported. This describes the implementation process, not legal clearance.
