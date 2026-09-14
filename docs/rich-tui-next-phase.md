# Rich TUI next phase: reusable surface

**Status:** Accepted through ADR-0014 as protocol and host design. A first-party
task cockpit is not shipped in this tree. External usefulness gates remain open.
**Date:** 2026-08-18
**Starting point:** protocol 0.2 styled runs, cell-rect and
viewport attachments, explicit rich focus, mux-side overlay snapshots, and
classic fallback (`prismattyc-rich-client`).
**Not a claim:** Rich mode is a future product feature.

## Outcome and material wedge

Build one useful application and extract each host capability it needs into a
reusable rich-TUI surface.

The intended first-party app is a local failure-triage cockpit for several
repository tasks: a repo-local manifest of named commands, start only on
explicit user action, retain run state, index diagnostics, cancel and rerun.
It is not in this repository.

That cockpit is not valuable merely because it saves a command name. Its material
case is this:

> Keep the status of several concurrent tasks, the selected diagnostic, and
> the available actions visible while ordinary task output remains a normal,
> selectable terminal transcript. After detach and reattach, recover that
> information hierarchy without rerunning work or reconstructing which pane
> contained which failure.

Existing tools cover parts of that task. Command runners provide names;
watchers focus one command; test runners understand one domain; panes preserve
separate terminals; full-screen TUIs can draw a dashboard. They do not jointly
provide a stable cross-run semantic overview while retaining the terminal's
ordinary transcript, host selection, scrollback, and detach behavior.

Rich mode therefore does not replace classic output. The app would emit task
lifecycle and output as ordinary PTY text. Prismattyc reserves a bounded, pinned
workspace region above that guest grid and resizes the PTY into the remaining
rows, so the workspace covers no transcript cells. The workspace shows the
task overview, bounded diagnostic index, selected context, and actions.
Classic mode renders the complete task in an ANSI TUI and does not depend on
primary-screen scrollback.

This is a hypothesis, not a guaranteed product win. The phase has a stop rule:
compare the same concurrent-failure task in rich and classic modes. If rich
mode does not materially reduce view changes, lost-context recovery, or task
completion time without increasing errors, stop after the reusable client and
core fabric. Do not fund decorative outcomes 4 and 5 merely because the
compositor can draw them.

## Alternatives and boundaries

| Existing approach | What it already does well | Gap this phase tests |
|---|---|---|
| `just` / `make` | Names and runs commands | No persistent cross-run state or semantic diagnostic index |
| `bacon` / `cargo-watch` | Repeats and displays a focused Rust command | Does not coordinate heterogeneous explicit tasks and prior runs |
| `cargo-nextest` | Strong test execution and reporting | Test-specific, not a repo task/service cockpit |
| mux panes / `tmux display-panes` | Concurrent terminals and durable output | User must arrange panes and reconstruct state across them |
| ordinary full-screen TUI | Can implement the whole workflow in text | Takes over the screen and must recreate selection, history, and composition |

An ANSI Runbook remains a valid product. If the rich comparison does not show
a material benefit, the correct outcome is to keep that ANSI application and
stop expanding the protocol.

This application is not a general IDE or orchestration system. It does not own
agents, tickets, CI providers, worktrees, remote execution, or editor launch.
The host never interprets an opaque app action as a command.

## Frozen application scope

Version 1 uses one explicitly opened manifest at `.prism/runbook.toml` in the
current repository root. It does not search parent directories or combine
multiple roots.

- A task contains structured `argv`, an explicit or repository-relative `cwd`,
  and bounded environment overrides. Manifest values are never shell strings.
- Different tasks may run concurrently. One task has at most one active run;
  rerun starts only after the prior process group is terminal.
- Opening a manifest, restoring a view, or reattaching never starts work.
- The Runbook child owns task definitions, process supervision, run history,
  diagnostic parsing, and the complete classic and rich presentations.
- Cancellation targets the owned process group and reports graceful versus
  forced termination. A Runbook death guard also owns every task process
  group: pane close, Runbook crash, or PTY generation end sends graceful
  termination, waits the frozen grace interval, forcibly kills survivors, and
  reaps them before the generation is considered closed. No task grandchild is
  allowed to survive in the mux or user session.
- Numeric caps for task count, concurrent processes, retained output, kill
  grace, and environment size are frozen in the ADR after a baseline. OS-level
  CPU and memory sandboxing, multi-root discovery, concurrent same-task runs,
  and persistent history are explicitly deferred.

## Experience sketch

The exact visuals remain unfrozen, but the hierarchy is concrete:

```text
┌ Runbook · Prism ───────────────────────────────────────────────────────────┐
│ Tasks                         │ Selected run: test                         │
│ ● check       passed   0:08   │ running  182/420  ███████░░░  00:12       │
│ ▶ test        running  0:12   ├────────────────────────────────────────────┤
│ ○ clippy      ready           │ test renderer::resize_preserves_regions…  │
│ ○ dev-server ready           │ error[E…] at crates/…/src/…rs:142         │
│                               │   140 │ …                                  │
│ History                       │ > 142 │ …                                  │
│ test     failed      09:41    │   143 │ …                                  │
│ check    passed      09:39    │                                            │
├───────────────────────────────┴────────────────────────────────────────────┤
│ Enter run · x cancel · r rerun · / filter · Tab focus · C-S-G rich focus │
└────────────────────────────────────────────────────────────────────────────┘
ordinary selectable PTY transcript continues in the reserved rows below
```

Wide panes show overview and detail together. Narrow panes show the selected
task first and expose the same actions through the keyboard. Color reinforces
state but never carries state alone. The sketch shows the pinned workspace,
not a full-pane takeover; Prism always leaves the negotiated minimum transcript
region beneath it or declines the rich workspace.

## Ownership and lifetime

The PTY child is the durable application owner within one live PTY generation:

```text
Runbook child
  owns tasks, child process groups, logs, history, diagnostics, scene source
       |
       v
public prismattyc-rich-client
  owns negotiation, generation, revisions, resnapshot, event validation
       |
       v
mux server or direct host
  owns PTY lifetime and a bounded scene cache; never owns Runbook actions
       |
       v
one or more viewers
  own focus, hover, pointer capture, selection, and viewport state
```

The public boundary remains mandatory:

- `prismattyc-protocol` owns dependency-light messages, capability identifiers,
  limits, revisions, and malformed-input behavior.
- `prismattyc-rich-client` owns negotiation, raw-TTY lifecycle, keyed-tree diffing,
  resnapshot, event validation, and graceful fallback.
- Host, mux, and render code own bounded scene state, layout, clipping, damage,
  hit testing, focus arbitration, and theme resolution.
- An independent application's Prism integration imports only the public
  client and protocol. Runbook's first-party package additionally reuses the
  renderer for deterministic presentation math and core's OSC 52 encoder; it
  cannot import private host, mux, compositor, or transport types.

The provisional capability families are tree/layout, collection patches,
keyboard events, pointer activation, virtual scroll, semantic text, and status
primitives. A peer negotiates each family independently. The ADR assigns exact
wire names and versions; this proposal does not freeze them.

The mux server already owns and supervises the generic PTY child. It may cache
the latest validated scene so a new viewer can paint promptly, but it does not
hold the Runbook model, choose tasks, or spawn task processes on the app's
behalf. Runbook's application-side death guard supervises its task process
groups and treats loss of the main Runbook process or PTY generation as an
implicit cancel-all with grace, forced kill, and reap.

A negotiated capability grant is bound to the PTY generation. Viewer detach
does not revoke it. Reattach restores the cached display, then requests or
accepts a current app snapshot; it never starts or reruns a task. Rich focus is
viewer-local and always revoked by detach. Child exit ends the generation. A
host or server generation change requires negotiation and a full snapshot.
Cold resurrection after server or child death is not part of this phase.

## Surface, revisions, and failure domains

Runbook uses one negotiated `workspace` surface per pane. Rail, selected detail,
and footer are nodes in that surface, not independently positioned
attachments. This keeps layout atomic and makes the rich failure boundary
clear: a malformed structural scene drops the workspace and Runbook continues
in classic mode.

The workspace uses a new **reserved-chrome attachment**, not a cell rectangle
or a viewport overlay. The controlling viewer resolves a bounded dock extent;
Prism subtracts that extent from the pane before sizing the guest PTY and sends
the resulting rows and columns through the normal resize path. The workspace
therefore stays pinned without covering guest cells. A mux attach that is not
the pane controller clips or pads the controller's logical geometry rather
than silently choosing a second PTY size.

The ADR must supersede ADR-0013 and the attachment choice in
`hybrid-rendering.md` D5. It must freeze maximum dock extent, minimum transcript
rows, resize ordering, and the too-small fallback. In passive mode, wheel over
either region scrolls pane history; grid selection starts in the uncovered
transcript, and a drag crossing the dock is clamped to transcript cells until
semantic selection exists. `Shift` remains the unconditional host-selection
escape. Resize resolves the dock first, then the PTY. If both the dock and the
minimum transcript cannot fit, Prism declines or drops the workspace and gives
the full pane back to the guest.

Virtual collections inside the workspace have independent identities and
revisions. The client is the sole writer:

- `surface_generation` distinguishes a new child/application lifetime.
- `scene_rev` orders structural snapshots and patches.
- `(collection_id, collection_rev)` orders each task, diagnostic, or history
  window independently.
- The host ACKs, rejects, requests a bounded resnapshot, or drops the affected
  collection/surface. It never invents a revision.
- Replaceable status updates may be coalesced only when explicitly marked.
  Append-only log and diagnostic records are ordered and never silently
  coalesced.
- Scene, collection, patch, queue, text, and update-rate limits are advertised.
  Exceeding a limit degrades the affected rich surface instead of blocking PTY
  parsing or classic paint.

Collection content is application-global, but its visible window is
**viewer-local**. The cached scene is a viewer-neutral template. Each attached
viewer receives an opaque generation-scoped `viewer_id` and owns follow/pause,
scroll offset, local filter, focused item, hover, and rich focus. View events
carry that identity. Viewer-local changes neither advance a shared collection
revision nor overwrite the cached template; any app-computed projection is a
transient viewer-addressed reply with its own request sequence. Shared actions
such as run or cancel still mutate Runbook state and produce ordinary shared
client-authored revisions. Detach discards the viewer state, so a second attach
cannot move the first viewer's collection window.

## Scroll and screen ownership

There are two distinct scroll domains:

1. **Pane scrollback** belongs to Prism/mux and contains the ordinary PTY
   transcript. In passive rich mode, wheel and host scroll commands always act
   here.
2. **Virtual collection scroll** is a viewer-local projection over
   Runbook-owned collection content. It moves only when rich focus is granted
   and keyboard focus or the pointer targets a scrollable collection. Its
   window request sequence is independent of shared collection revisions and
   pane history.

Rich mode remains on the primary screen so normal output and host selection
remain available. Runbook enters the alternate screen for its complete classic
ANSI presentation only when rich negotiation fails at startup, rich mode is
disabled before activation, or the user explicitly chooses that transition.
A mid-generation surface failure removes the dock, restores the full guest
grid, emits one bounded degradation record, and continues with a line-oriented
classic status/action prompt on the primary screen; it never hides the existing
transcript by entering the alternate screen automatically. Classic mode retains
bounded history in the Runbook process, redraws the full current state after
attach/resize, and never relies on evicted primary scrollback. If another
program state enters the alternate screen while a rich workspace exists, the
existing Prism policy suspends the workspace.

The live acceptance path includes `prismattyc-mux-server --experimental-rich`,
Runbook as a mux pane, `prism-mux-attach`, pane scroll, collection scroll,
detach, reattach, resize, and the classic alternate-screen path.

## Input consent state machine

Pointer activation never bypasses rich-focus consent.

| State | Keyboard | Click and wheel | Entry or revocation |
|---|---|---|---|
| Passive | Goes to the normal PTY; host chords remain authoritative | Click may focus the pane but cannot activate an app action. Drag and wheel remain host selection/scroll | `Ctrl+Shift+G` grants rich focus after an explicit host gesture |
| Rich focus | Structured key events go to Runbook except reserved host chords | Release on the same interactive node activates it; wheel over a rich collection scrolls that collection | `Esc`, `Ctrl+Shift+G`, pane switch, detach, surface failure, or generation change revokes |

In rich focus, a primary-button movement beyond the click threshold cancels the
pending app activation and returns the gesture to host selection. `Shift` drag
is an unconditional host-selection escape. Actions fire only on a same-node
release below the threshold; a press alone has no effect.

Every event carries `surface_generation`, `viewer_id`, `scene_rev`, `node_id`,
and the relevant collection revision or viewer-local window request sequence.
Runbook rejects events for a stale generation or viewer, removed node, or
outdated action binding. The host may return an opaque action ID, but it never
opens a file, URL, or process for the app.

## Five user-facing outcomes

These remain the five user-visible outcomes requested for the rich experience.
They are not five parallel ticket boundaries. Each is backed by an independently
negotiable capability family and lands in dependency order.

### 1. Adaptive workspace layout

**Experience.** Task overview, selected run, diagnostic context, and action
footer adapt between wide, narrow, and minimum pane sizes without changing the
logical selection.

**Fabric.** A bounded keyed tree provides row, column, stack, text, border,
spacer, and collection nodes; minimum/preferred/fill sizing; alignment;
clipping; and host-resolved viewport geometry.

**Acceptance.** Deterministic fixtures cover wide, narrow, minimum, and resize
cases. Nodes cannot cross the pane or host chrome. An invalid tree drops only
the workspace, preserves the classic grid, and leaves the Runbook child alive.

### 2. Virtualized task, run, and diagnostic collections

**Experience.** Several active tasks and bounded histories remain responsive.
Users follow or pause a selected run, filter diagnostics, inspect context, and
return to the current result without repainting the whole workspace.

**Fabric.** Independent collection revisions provide ordered append,
replaceable update, removal, bounded windows, overscan, ACK/reject, and
resnapshot. Follow, pause, filter, and virtual scroll are generic collection
behavior—not a host-side Runbook log widget.

**Acceptance.** A flood fixture mixes PTY text, status replacement,
append-only diagnostics, collection scroll, a revision gap, and resnapshot. It
proves bounded queues and memory, ordered records, independent collection
recovery, and responsive classic paint.

### 3. Direct keyboard and pointer operation

**Experience.** With explicit rich focus, users navigate tasks, run, cancel,
rerun, filter, select a diagnostic, and scroll a rich collection. Every action
has a keyboard path and visible focus.

**Fabric.** Structured key, pointer, wheel, activation, focus, and viewport
events follow the consent state machine above. Capability negotiation separates
keyboard, pointer activation, and virtual-scroll support so a host can grant a
safe subset.

**Acceptance.** State-machine and live-host tests cover passive click, explicit
grant, same-node release, drag cancellation, selection escape, wheel routing,
reserved host chords, stale events, pane switch, detach, and reconnect. Input
cannot reach the wrong pane, generation, node, or action.

### 4. Semantic diagnostics, selection, and copy

**Experience.** Errors and warnings are searchable and selectable. Users copy
a diagnostic, command, or `path:line:column` as plain text without decorative
glyphs. Opening an editor or source location is deferred; Runbook only displays
and copies the location.

**Fabric.** Semantic roles and logical text ranges cover heading, label, value,
status, code, diagnostic severity, and inert location metadata. Selection uses
logical text order across styles and virtual rows. A deterministic plain-text
projection is required now for selection, clipboard export, tests, and future
accessibility adapters.

**Acceptance.** Fixtures cover wrapping, wide characters, mixed styles,
filtered virtual rows, cross-run selection, clipboard serialization, text
projection, and stale-range rejection. Live proof covers OS clipboard and
visual selection in the windowed host and mux attach.

### 5. Semantic status and progress visualization

**Experience.** Task state, elapsed time, real progress, recent outcomes, and
unseen changes are readable through badges, meters, and a bounded history
trend. Text remains authoritative.

**Fabric.** Theme tokens resolve through the active Prism theme. The only new
data primitives are a status badge, determinate/indeterminate meter, and
bounded sparkline. Progress comes from app-reported units or process state;
Prism never invents a percentage.

**Acceptance.** Golden scenes and live screenshots cover themes, contrast,
reduced motion, narrow clipping, and state not encoded by color alone. After
Runbook becomes quiet, both a direct host and a mux-attached idle pane have no
animation timer, repaint, or idle CPU loop.

## Dependency-ordered fabric cut

Waypoint work follows this accepted dependency order:

0. **Superseding ADR and fixtures:** replace ADR-0013 and D5 with the ownership
   model, reserved-chrome attachment, capability families, viewer-local
   windows, consent state machine, revision rules, death policy, limits,
   fallback, and byte fixtures.
1. **Public client extraction:** 0.1/0.2 negotiation and lifecycle live in
   `prismattyc-rich-client`; prove existing behavior byte-identical.
2. **Workspace tree and layout:** land outcome 1 and a read-only Runbook slice.
3. **Revisioned collections:** land outcome 2 and concurrent task/diagnostic
   state; measure the classic baseline and reassess the material wedge.
4. **Structured events:** land outcome 3 only after the consent model has
   mutation and live pointer tests.
5. **Semantic text:** land outcome 4 on the stable tree, collection, and event
   contracts.
6. **Status primitives:** land outcome 5 only if the measured wedge justifies
   continuing.
7. **Integrated dogfood and authoring docs:** run direct-host, TTY, mux-attach,
   detach/reattach, failure, flood, resize, theme, clipboard, and idle proofs.

Do not create five parallel implementation tickets. Each implementation ticket
must be a vertical slice across protocol, host/core/render, public client,
Runbook behavior, negative paths, and documentation.

## Definition of useful and evidence

The early continuation gate at delivery step 3 uses the four-task fixture but
stops at identifying the first actionable diagnostic and recovering task state
after detach/reattach. It compares view changes, lost-context recovery, time to
diagnosis, and wrong selections in rich and classic modes. It does **not**
require semantic copy, pointer activation, or outcome-4 behavior. That reduced
comparison decides whether to fund later interaction and decoration work.

The final integrated comparison starts four heterogeneous tasks, includes
simultaneous output and at least one failure, detaches and reattaches a viewer,
identifies the first actionable diagnostic, copies `path:line:column` plus
context, reruns the failed task, and confirms the final state.

Run the same deterministic fixture in rich and classic modes. Record:

- successful completion and wrong-action count;
- view or pane changes;
- time spent reconstructing task state after reattach;
- time to the first correct diagnostic and final confirmation;
- user preference and the reason for it.

Set a numeric continuation threshold only after measuring the classic baseline.
A screenshot proves rendering, not usefulness. Internal Runbook success can
justify another experimental fabric cut, but PRD §5.6 still requires external
discovery, independent integration, and differentiator-value evidence before a
production-rich claim.

## Cross-cutting contract

- Capability query succeeds before rich output; no reply means classic.
- Protocol 0.1 and 0.2 behavior remains byte-compatible.
- Classic fidelity, selection, scrollback, and host chrome remain the release
  gate. Rich failure cannot blank or freeze the grid.
- The Runbook child owns app state; the server owns only bounded transport and
  scene cache; viewers own local interaction state.
- All protocol and retained-state dimensions have advertised hard bounds.
- Start, cancel, and rerun require explicit app actions. Restore and reattach
  are display operations only.
- No log, command output, path, environment value, or diagnostic is uploaded to
  VectorVault or another service.
- Exact-head validation includes parser/property tests, real PTYs, the Phase 3
  rich harness, deterministic offscreen rendering, Termwright where applicable,
  and inspected windowed-host screenshots.

## Review-response ledger

| Feedback | Revision |
|---|---|
| Review B1 / N3: thin wedge and missing alternatives | Added the transcript-plus-semantic-workspace wedge, alternatives table, comparison metrics, and stop rule |
| Review B2: click consent undefined | Added a two-state consent table; passive clicks never activate; click requires rich focus; drag and `Shift` preserve selection |
| Review B3: five parallel generations | Kept five user outcomes but defined capability dependencies and an ordered 0–7 delivery cut |
| Review B4: app/server ownership contradiction | Made the Runbook child the sole app/process/log owner; server holds only bounded scene cache |
| Review B5 / N5: mux attach and alternate-screen fallback absent | Defined separate pane/collection scroll, rich primary-screen mode, complete classic alt-screen mode, and live attach/detach acceptance |
| Review B6: accidental §5.6 closeout | Stated in status, outcome, evidence, and contract that internal dogfood leaves the production checkpoint open |
| Review N1: source action has no effect | Reduced it to display/copy of `path:line:column`; editor launch is deferred |
| Review N2: idle animation risk | Added direct-host and mux-attached no-timer/no-repaint acceptance |
| Review N4: app limits unset | Froze v1 discovery and concurrency rules; named numeric/resource items for the ADR and deferred OS sandboxing |
| Review N6: accessibility inconsistency | Required a deterministic plain-text semantic projection now without claiming an OS accessibility adapter |
| Fabric: scroll and attachment ownership | Chose one workspace surface, independent collection revisions, and explicit pane versus collection scroll domains |
| Fabric: capability lifetime and stale input | Bound capabilities to PTY generation; added resnapshot behavior and generation/revision IDs to every event |
| Fabric: append data and revision authority | Made the client sole writer; prohibited coalescing append-only records; defined ACK/reject/resnapshot/drop outcomes |
| Re-review B1 / N1 / N2: workspace geometry and drop path | Chose reserved chrome that shrinks the guest PTY, leaves a minimum selectable transcript, supersedes ADR-0013/D5, and never auto-enters alt-screen after a mid-session rich drop |
| Re-review B2: multi-viewer window contradiction | Made shared collection content client-authored but every visible window viewer-local, with generation-scoped viewer identity and non-cached projection replies |
| Re-review B3: generation-end process cleanup | Required an application-side death guard to grace, force-kill, and reap every owned task group on cancel, crash, pane close, or generation end |
| Re-review N3 / N4: table label and premature copy metric | Renamed the consent column and split the reduced step-3 continuation gate from the final outcome-4 copy comparison |

## Explicitly deferred

- General canvas, images, arbitrary shaders, HTML/CSS/JS, and plugin execution.
- Remote execution, CI-provider integration, agent/ticket/mail orchestration,
  editor launch, and automatic worktree management.
- Parent-directory manifest discovery, multi-root workspaces, concurrent runs of
  the same task, persistent history, and OS-level resource sandboxing.
- Cold resurrection that replays commands.
- A production-rich claim until every applicable PRD §5.6 gate has evidence.
