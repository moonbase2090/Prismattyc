# A-6 pilot record — operator-b, 2026-08-12 (Role=pilot, EXCLUDED)

**This row scores nothing toward the A-6 cohort bars.** Operator is a Prism
reviewer/implementer-adjacent agent (PRD §1.6 exclusion) and a scripted
control-plane client, not a discoverable-UI session operator. Recorded under
the wrap's "owner dogfood / pilot rows welcome" allowance.

- **Operator:** operator-b (Claude Code agent), Hive cell 16@1, running
  *inside* a live single-pane `prismattyc-host` (pid 2011709) — the pilot mux
  server was a separate scratch instance.
- **Build under test:** `origin/main` @ `b3a384c` (isolated worktree,
  `cargo build --release -p prismattyc-mux --bins`).
- **Path:** 2B `prismattyc-mux-server` + ADR-0008 control plane over the same-user
  Unix socket. No GUI. Driver: scripted JSON client (`pilot_driver.py`).
- **Comparison baseline:** tmux control mode / `tmux send-keys` +
  `capture-pane` scripting, which this operator drives daily from agent seats.

## Step results (§5.6.1)

| Step | Result | Notes |
|------|--------|-------|
| 1 — three panes | **Done** (0.00s) | Two `split` mutations (vertical then horizontal) from one client; snapshot confirms 3 leaves |
| 2 — discover focus/resize | **Not scorable from this seat** | Operator read the source; cannot honestly measure one-page-card discoverability |
| 3 — distinct markers | **Done** (1.00s incl. settle) | Per-pane `acquire_lease` → `write_pane` (`echo A6_MARKER_n_<pid>`) → `read_pane`; all three markers visible in server-owned grids |
| 4 — copy history/output | **Blocked (GUI-side)** | Host selection/clipboard is windowed-host chrome; control plane has no clipboard verb. `read_pane` text extraction works but is not the step as frozen |
| 5 — close pane + reflow | **Done** (0.05s) | `close` on middle pane; snapshot shows 2 leaves; both survivors' grids still carry their markers (no corruption) |
| 6 — detach | **Done** (0.50s incl. settle) | `disconnect_client` + socket close; server and all pane children stayed alive |
| 7 — reattach | **Done** (0.00s) | Fresh connection + `register_client`; identical pane topology; markers intact in every surviving grid |
| 8 — session switch | **Blocked (not implemented)** | Snapshot exposes `sessions[{id, name:"default"}]` but the control plane has no create/switch-session verb (verbs: snapshot/events/register/leases/write/read/split/close/resize/suggest_focus). Expected result unchanged; recorded as gap, not failure |
| 9 — locate unseen output | **Partial** | Event stream carries topology/lease kinds only (`pane_split`, `pane_closed`, `geometry_changed`, `focus_suggested`, `lease_changed`) — **no output-activity event**. Background output is locatable only by `read_pane` polling from this seat; the unseen badge is GUI chrome, untested here |

**Wall-clock, scripted steps 1+3+5+6+7+9: 1.56s total.** (Not comparable to a
human operator's time; recorded for the scripted-client profile only.)

## Misroutes and recovery

1. **Stale ambient socket (real, pre-existing):** `/run/user/1000/prism-default.sock`
   existed with a pid file (729123) whose process is dead; the server log ends
   in a spawn error (`Unable to spawn "--"` — argv mishandling in whatever
   invocation produced it). A fresh operator attaching to the advertised
   default socket would hang or fail with no liveness hint. Recovery: checked
   pid liveness by hand, started a fresh server on a scratch socket.
   *Suggestion candidate: liveness-check + clear error on stale socket attach.*
2. **Driver bugs (mine, not the server's):** snapshot nests windows under
   `sessions[0]` (assumed flat), and events arrive as `batch.events` (read
   `events`). Both were immediately diagnosable from the JSON — the typed
   envelopes made the errors obvious.
3. **`snapshot_required` on a fresh connection** before `events` is a
   first-class, self-describing error (`resnapshot_required: true` with
   sequence bounds) — correct behavior, zero recovery cost.

## Versus current externals (tmux control mode, Herdr; same scripted task list)

- **Material advantage (for this scripted-agent profile):** typed JSON
  snapshot/mutation protocol with stable opaque IDs, first-class stale-sequence
  errors, and per-pane writer leases. The equivalent tmux flow is string-parsed
  `list-panes`/`capture-pane` output with index-based targets that shift on
  close — the exact misroute class steps 5–7 probe. Lease semantics (observer
  write rejected, takeover explicit) have no tmux equivalent short of
  server-side discipline.
- **Equal capability:** detach durability (tmux does this too, reliably).
- **Prism gaps vs externals:** no session create/switch verb (step 8; tmux has
  sessions), no output-activity event (step 9; tmux has `monitor-activity`),
  no clipboard path off-GUI (step 4).
- **Would-you-drop:** **Not yet** for daily driving — session verbs and an
  activity/output event are the two blockers for this operator's real
  workflows. For *programmatic* pane orchestration under an agent, the typed
  control plane is already preferable to tmux scraping.

## Verdicts this record does NOT support

No §5.6.1 completion is claimed. Cohort count remains **0 eligible**. This
record must not be cited as an A-6 completion, pass, or operator-value
evidence beyond the pilot-row allowance in the evidence pack.
