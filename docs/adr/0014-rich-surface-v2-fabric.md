# ADR-0014 — Rich surface v2 and Runbook boundary

**Status:** Accepted (design freeze; implementation remains staged)
**Date:** 2026-08-17
**Supersedes:** ADR-0013 for protocol `0.3` and later
**Preserves:** protocol `0.1` and `0.2` byte behavior; classic-grid P0 priority;
query-before-emit
**Plan:** [Rich TUI next phase](../rich-tui-next-phase.md)
**Fixtures:** [`rich-surface-v2`](../fixtures/rich-surface-v2/README.md)

## Context

ADR-0013 deliberately stopped at styled runs in cell rectangles and viewport
overlays. That was sufficient to prove bounded APC transport, host and mux
composition, focus consent, and classic fallback. It is not sufficient for a
useful application that keeps a pinned semantic workspace visible while an
ordinary selectable transcript remains in the same pane.

The first application is `prismattyc-runbook`: a local repository task cockpit.
It is also the forcing function for a reusable public client and host surface.
This ADR freezes the decisions that must precede another protocol
implementation cut.

This decision does not claim that rich mode is a product differentiator.

## Decision

### 1. Compatibility profiles and capability cut

Protocol `0.3` is the first rich-surface-v2 profile. Minor-version negotiation
remains additive. A `max=0.1` or `max=0.2` query receives the exact existing
reply bytes; no new feature or limit key may leak into a downgraded reply.
The locked byte fixtures are executable in `prismattyc-protocol` tests.

The `0.3` feature registry adds these independently grantable identifiers:

| Identifier | Grant |
|---|---|
| `hybrid.reserve.rows` | One top-docked workspace that reserves rows outside the guest grid |
| `rich.tree.v1` | Bounded keyed tree snapshots and structural patches |
| `rich.collection.v1` | Revisioned shared collection content and viewer projections |
| `input.rich_keyboard.v1` | Structured keys while existing rich focus is granted |
| `input.rich_pointer.v1` | Press/move/release activation under the consent state machine |
| `input.rich_scroll.v1` | Viewer-local collection wheel and viewport requests |
| `rich.semantic_text.v1` | Logical text, roles, ranges, and plain-text projection |
| `rich.status.v1` | Badge, meter, and bounded sparkline primitives |

`input.rich_focus` remains the prerequisite consent capability. Keyboard,
pointer, and collection scroll stay separate so a host can grant a safe subset.
`rich.collection.v1`, `rich.semantic_text.v1`, and `rich.status.v1` require
`rich.tree.v1`. A reserved workspace requires both `hybrid.reserve.rows` and
`rich.tree.v1`. `markup`, `animation`, and `canvas` remain unadvertised.

The canonical `0.3` capability reply advertises the limits below. The exact
body and field order are locked in `capability-v0.3.tsv`.

| Limit key | Value | Meaning |
|---|---:|---|
| `limit.body` | 4096 bytes | Existing maximum APC body |
| `limit.surfaces` | 1 | Workspace surfaces per PTY generation |
| `limit.nodes` | 128 | Nodes in the validated shared tree |
| `limit.tree_depth` | 16 | Maximum parent depth |
| `limit.collections` | 16 | Shared collections per surface |
| `limit.collection_items` | 2048 | Retained items per collection |
| `limit.patch_ops` | 128 | Operations in one structural or collection patch |
| `limit.queue` | 256 | Pending validated rich messages per pane |
| `limit.retained_text` | 2097152 bytes | Shared rich text retained per surface |
| `limit.event_rate` | 240 per second | Accepted structured input events per viewer |
| `limit.dock_rows` | 24 | Absolute top-dock row ceiling |

The body limit applies to every message even when another retained-state limit
is larger. Snapshots and ordered append records may span messages only through
an explicit revisioned sequence; there is no implicit APC concatenation.

### 2. Reserved workspace geometry

Rich v2 adds one attachment kind: **top reserved rows**. It is neither a
cell-rect attachment nor a viewport overlay.

For pane content size `H × W`, the application requests
`min=5, preferred<=24, max<=24` rows. The host grants only when `H >= 13` and
`W >= 40`. The chosen dock `D` must satisfy all of:

```text
5 <= D <= 24
D <= floor(3 * H / 5)
H - D >= 8
```

The controlling viewer chooses `D` within the requested and host bounds.
Prismattyc subtracts `D` before sizing the guest PTY, then sends the resulting
`H-D × W` through the normal resize/SIGWINCH path. Workspace paint is clipped
to the dock; guest paint and grid selection are clipped to the remaining grid.
No rich pixel or cell covers a transcript cell.

Direct `prismattyc-host` has one viewer, which is the controller. For a mux pane, the
connection holding the existing connection-bound controller lease is the only
viewer allowed to change PTY or dock geometry. Observers render the controller's
logical geometry with clipping or padding. Controller transfer uses the
existing lease protocol; the old geometry remains until the new controller's
first accepted resize.

In passive mode, wheel over either region scrolls pane history. Grid selection
begins only in the transcript; a drag crossing the dock clamps to transcript
cells until semantic selection is available. `Shift` remains an unconditional
host-selection escape. If the minimum dock and eight transcript rows cannot fit,
or width is below 40, the host declines the workspace and gives the full pane
to the PTY.

Dropping an active workspace removes the reservation first, restores the full
guest size, and sends SIGWINCH. Runbook emits one bounded degradation record and
continues with its line-oriented classic prompt on the primary screen. It must
not automatically enter the alternate screen and hide the existing transcript.
An alternate-screen classic UI is allowed only when startup negotiation never
granted rich mode, rich mode was disabled before activation, or the user
explicitly requests the transition.

### 3. Ownership and lifetime

Ownership is intentionally split:

| State | Sole owner | Cache or projection |
|---|---|---|
| Tasks, runs, diagnostics, action bindings | Runbook process generation | None in host |
| Shared tree and collection content revisions | Public client in Runbook | Latest validated bounded snapshot in host/mux |
| PTY lifetime and controller lease | Direct host or mux server | Server durable across viewer detach |
| Viewer focus, hover, selection, follow, filter, scroll, focused item | One viewer connection | Never written into the shared scene cache |

Capability grants, surface generations, and shared revisions are bound to the
PTY generation. Viewer detach does not restart tasks or revoke the application's
grant, but it always destroys that viewer's focus and projection state. Child
exit or PTY generation change destroys the shared surface and requires a new
query plus full snapshot.

The host or mux mints each `viewer_id` from 128 bits of cryptographically random
data and renders it as 32 lowercase hex characters. It is bound to one viewer
connection and one PTY generation. The child cannot choose an ID. A
viewer-addressed response is accepted only for an outstanding host-minted
`(viewer_id, request_seq)` on that same connection; unsolicited or cross-viewer
responses are dropped. Reattach always receives a new ID.

The mux cache is a viewer-neutral template: shared tree, shared collection
content, action bindings, and their revisions. A viewer window is a local
projection. When Runbook must compute a filter or window, the reply is transient,
addressed to the outstanding viewer request, and never replaces the cached
template or another viewer's window.

### 4. Revisions, acknowledgements, and failure domains

The public client is the only writer of shared revision numbers:

- `surface_generation` is a non-zero `u64` for one application/PTY generation.
- `scene_rev` is a non-zero `u64` for the shared keyed tree.
- `(collection_id, collection_rev)` orders shared collection **content**, not a
  viewer's visible window.
- `(viewer_id, request_seq)` orders one viewer's transient projection requests.

A structural or collection patch names its base and next revision. The next
revision must be exactly current + 1. The receiver produces one of:

| Condition | Receiver action |
|---|---|
| Full valid snapshot for current generation | Install atomically; ACK installed revision |
| Patch base equals current and next is current + 1 | Apply atomically; ACK next revision |
| Exact duplicate revision and payload hash | Do not reapply; ACK current revision |
| Same revision with different payload | Reject `conflict`; drop affected collection/surface; request snapshot |
| Base behind or ahead of current | Reject `stale` or `gap`; request bounded snapshot |
| Invalid node, bound, generation, or reference | Reject; drop only the affected collection when isolation is possible, otherwise the surface |
| Queue or retained-state bound exceeded | Reject without blocking PTY parsing; request snapshot or drop affected rich state |

Status replacements may coalesce only when explicitly marked replaceable and
no observer-visible intermediate state is required. Log, history, and diagnostic
appends are ordered and never silently coalesced. When their queue is full, the
receiver rejects the append and requests a collection resnapshot; it does not
pretend that a later revision includes the missing record.

All parsing, validation, state installation, ACK/reject, and cache mutation is
bounded. A rich failure cannot block classic PTY parsing or paint.

### 5. Input consent and stale-event rules

Rich v2 retains the shipped `Ctrl+Shift+G` grant chord.

| State | Behavior | Revocation |
|---|---|---|
| Passive | Keys go to PTY. Click may focus the pane but cannot activate an app action. Drag and wheel remain host selection/scroll. | Not applicable |
| Rich focus | Granted structured capabilities may emit events. Same-node release below threshold activates. | `Esc`, grant chord, pane switch, detach, surface failure, controller loss, or generation change |

The host classifies a drag before emitting an activation: movement of at least
4 logical pixels in a pixel-addressed host, or one cell in a cell-only attach,
cancels activation and returns the gesture to host selection. `Shift` drag
always selects. Press alone never acts. Every structured event carries the
host-minted `viewer_id`, surface generation, current scene revision,
node/action ID, and relevant collection revision or window request sequence.

The host drops an event before delivery when any identity or revision is stale,
the viewer is not bound to that connection, the node/action was removed, rich
focus is absent, or the independently negotiated event capability is absent.
Runbook repeats those checks before mutation. Reserved host chords are never
forwarded as rich events.

### 6. Runbook process-group death policy

Every task runs under a dedicated **task guard**. The guard is the direct parent
of the command, while the command is leader of a new process group containing
its descendants. The guard remains outside that group so it can signal and
reap it. The guard is a separate process from the Runbook UI/model process, so
cleanup does not depend on a hook in the process that crashed.

The guard receives a close-on-exec liveness pipe whose write end is held only by
Runbook. Cancel is an explicit guard request. Runbook exit or crash closes the
pipe; pane close and PTY generation end terminate Runbook and therefore close
it. On cancel, pipe EOF, SIGHUP, or SIGTERM, the guard:

1. sends SIGTERM to the entire owned task process group;
2. waits up to **2000 ms** while reaping;
3. sends SIGKILL to surviving members;
4. reaps the direct child and exits only after the group is empty.

On Linux the guard also uses `PR_SET_PDEATHSIG=SIGTERM` as a redundant wake;
the close-on-exec pipe is the portable authority. The pane host allows **3000
ms** from cooperative termination before its final kill, leaving the guard's
2-second window plus scheduling margin. Runbook monitors every guard;
unexpected guard loss while its task is nonterminal disables new runs, kills
the recorded task group, and terminates the generation. No command is launched
without an acknowledged guard and no task grandchild may survive the pane
generation.

Runbook v1 application bounds are 64 manifest tasks, 8 concurrent task guards,
8 retained runs per task, 8 MiB captured output per run, 64 environment
overrides totaling at most 64 KiB, and 256 argv elements totaling at most 64
KiB. Output still streams as ordinary tagged PTY text; retained rich state is
subject to the smaller host-advertised limits above.

### 7. Public boundary and delivery order

`prismattyc-protocol` owns dependency-light message types, feature identifiers,
limits, and codecs. `prismattyc-rich-client` owns query lifecycle, raw TTY, shared
revision authority, resnapshot, event validation, and graceful fallback.
Host/mux/render code owns grants, geometry, validation, bounded cache, damage,
hit testing, viewer identity, and viewer-local projection state.
An independent application's Prismattyc integration needs only the public client
and protocol crates. Runbook's wire/session adapter follows that boundary;
the first-party Runbook package also reuses `prismattyc-render` for deterministic
offscreen presentation math and `prismattyc-core` for the existing OSC 52 encoder.
Neither dependency exposes host, mux, compositor, or task ownership to the
application. The compile-checked public `surface` example uses only the two
public integration crates.

Work lands in this order: public-client extraction with exact `0.1`/`0.2`
compatibility; reserved tree and read-only Runbook; collections and task guards;
structured input; semantic text/copy; optional status primitives after a
measured continuation gate; integrated proof and authoring guide.

Optional status is a separate revisioned layer keyed to authoritative text
nodes. It advertises only badge, determinate/static indeterminate meter, and a
16-sample sparkline. Semantic tones are host-theme inputs, not app colors; no
status record contains a clock, animation phase, or host-authored percentage.

## Rejected alternatives

- **Viewport overlay for the workspace:** pinned, but covers transcript cells
  and makes those cells unavailable to grid selection.
- **Cell-rect workspace:** preserves the grid but scrolls away, contradicting
  the material wedge.
- **App-global collection window:** lets one attach scroll another viewer.
- **Full-pane rich-primary takeover:** recreates terminal selection/history and
  is outside this phase.
- **Runbook-exit cleanup hook only:** cannot run after a crash.
- **Mux-owned Runbook model or task spawning:** collapses the public app/host
  boundary and makes the demo impossible for an independent client.

## Verification

Each later slice must add mutation and malformed-input tests before advertising
its feature. Required integrated evidence includes direct host and mux attach,
two simultaneous viewers, controller transfer, resize, detach/reattach, stale
generation/revision/viewer events, append flood, mid-session surface drop,
Runbook crash with a live grandchild, clipboard, and quiet-idle inspection.
