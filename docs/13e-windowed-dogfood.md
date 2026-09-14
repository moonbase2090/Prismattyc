# 13e — Windowed dogfood checklist

**Status:Done** 2026-08-13. Owner/implementer dogfood on
`origin/main` `f3162b6`. Formal A-6 PASS is **not** claimed. No new
fidelity-matrix row. Evidence (gitignored): `e2e/artifacts/13e-2026-08-13/`.

Nested Termwright remains the **classic / single-pane** harness. It does
**not** drive `prismattyc-host`. This checklist is for the windowed host plus the
2B control plane.

Not Phase 3. Not an A-6 PASS claim. Owner/implementer runs are **dogfood /
pilot** (PRD §1.6).

## Preconditions

- [x] `origin/main` includes (or dogfood the `feat/pm-44-mux-dogfood` tip)
- [x] `./scripts/test-phase2a.sh` green
- [x] `./scripts/test-phase2b-server.sh` green
- [x] `./scripts/test-phase2b-detach.sh` green
- [x] Current `pmuxd` socket handling replaces stale current `pmux*.sock`
      paths without selecting an unrelated socket
- [x] One-page card: [a6-operator-card.md](a6-operator-card.md)

## Windowed host (`prismattyc-host`) — §5.6.1 steps 1–5, 9

```bash
cargo run -p prismattyc-host -- --panes 3 -- /bin/sh
```

| Step | Do | Pass when |
|------|----|-----------|
| 1 | Three panes visible (or split `Ctrl+Shift+\` / `-`) | Three live PTYs — **PASS** (`prismattyc-host --panes 3`, title `3 panes`) |
| 2 | Discover focus + resize from the card only | `Alt+Arrow`; focus outline not color-only — **PASS** (card + cyan focus ring in chrome shots) |
| 3 | Distinct marker in each pane | Markers stay in the pane they were typed — **PASS** (`13E-PANE-<pid>`) |
| 4 | Copy from ≥1 pane | Drag + `Ctrl+Shift+C`; clipboard has the marker — **PASS** (`Ctrl+Shift+A` then `C-S-C`; clipboard held `13E-COPY-MARKER`) |
| 5 | Close one pane (`Ctrl+Shift+W`) | Reflow; remaining PTYs + markers intact — **PASS** (`3 panes` → `2 panes`; survivors kept markers) |
| 9 | Type in a background pane (or have it print) | Amber `!` / unseen count; focus clears it — **PASS** (amber `!` + title `2 unseen`) |

Capture at least one **desktop** and one **small** OS-window screenshot of
chrome (focus ring, unseen badge, header counts). Store under
`e2e/artifacts/13e-<date>/` if keeping evidence.

## 2B control plane — steps 6–8 (and 9 via events)

```bash
./scripts/prismattyc-mux-daemon.sh start -- /bin/sh
pmux-attach --write $'MAIN\n'
# detach (quit attach); children stay
pmux-attach                 # markers + child_alive
pmux new work                # create and attach to a work session
pmux attach default
# events after write to a background pane include output_activity
```

| Step | Do | Pass when |
|------|----|-----------|
| 6 | Quit attach / GUI | `pmux status` still reports the server; child alive — **PASS** (same child pid) |
| 7 | Reattach | Marker text + same child pid — **PASS** (`13E-MAIN` + pid unchanged) |
| 8 | Create `work`, then attach `default` | Two named sessions in snapshot; return restores first — **PASS** (`default` + `work`; return showed `13E-MAIN`) |
| 9 | Write on a non-focused pane; `Events` | One coalesced `output_activity` for that pane — **PASS** (one `output_activity` for the split background pane) |

If the default socket is leftover from a dead pid, attach must say **stale**
(not a generic connect error). Starting the server must replace it.

Stale diagnosis and replacement passed on a throwaway same-uid
`pmux-work.sock`. `pmux-attach` reported a stale control socket, `pmux up`
replaced it, and a subsequent write succeeded.

## Optional matrix rows

No new F-row is required to start 13e. If a windowed-only gap is found
(selection, hybrid mouse, scrollback chrome vs nested), file a **new** matrix
row ticket citing the ADR; do not silently widen `prismattyc-classic/0.1.1`.

## Out of scope

- Formal A-6 PASS / eligible-operator interviews
- Phase 3 rich / GPU
- Remote attach
- Making Termwright drive `prismattyc-host`
