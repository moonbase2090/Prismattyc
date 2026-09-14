# Roadmap

Status of the initial on-ramp defined in
[Prismattyc-Starting-Points.md](../Prismattyc-Starting-Points.md). Order is intentional:
protect classic fidelity, then open the rich surface.

## Now

| # | Item | Status | Notes |
|---|------|--------|--------|
| 0 | Private repo + charter + README | **Done** | Prismattyc supersedes Prism; this repo is the product |
| 0b | Docs tree (this directory) | **Done (v0 sketches)** | architecture, hybrid-rendering, capability-protocol, workspace, agents; on `main` @ f848861+ |
| 0c | Waypoint board Phase 0 | **Children done** | Epic and children all done; Phase 0A exit satisfied |
| 1 | Cargo workspace skeleton | **Done** | `4540d22` — see [workspace.md](workspace.md) |
| 1b | Basic CI (check, test, clippy) | **Done + live GHA green** | `5e8a951`; run `30417135305` @ `63dbac8` |
| 1c | Architecture decisions v0 | **Done on main** | — [decisions-v0.md](decisions-v0.md) (includes **D9** multi-core/GPU capacity) |
| 1d | Spike Baseline v0 | **Published / 0A exit** | [spike-baseline-v0.md](spike-baseline-v0.md) @ code head `e56d868` |
| 1e | Hybrid rendering freeze | **Done** | @ `63dbac8` — [hybrid-rendering.md](hybrid-rendering.md) |

## Next (build)

| # | Item | Status | Notes |
|---|------|--------|--------|
| 2 | Minimal viable classic terminal path | **Done (Phase 0A subset)** | on main `e56d868`; allowlisted gaps remain |
| 3 | High-level architecture formalization | Draft | [architecture.md](architecture.md) |
| 4 | Hybrid rendering model | **Done (freeze)** | [hybrid-rendering.md](hybrid-rendering.md) @ `63dbac8` |
| 5 | Capability / query protocol (first cut) | **0B spike closed** | types codec/attach @ `534284b` |

## Phase 0B (experimental)

| # | Item | Status | Notes |
|---|------|--------|--------|
| 5b | Experimental host flag + APC codec | **Done** | off by default; [phase-0b-spike.md](phase-0b-spike.md) |
| 5c | One cell-rect attachment + classic fallback | **Done** | `hybrid.attach.cell_rect` only |
| 5d | 0B safety harness | **Done** | 49 tests; live CI `30418912701` @ `534284b` |
| 5e | adversarial close | **Done (PASS)** | operator-b EXACT-HEAD PASS closed @ `534284b` |

## Phase 1 (first supported classic) — **shipped**

| # | Item | Status | Notes |
|---|------|--------|--------|
| 6a | Fidelity matrix v1 published | **Done** | [fidelity-matrix-v1.md](fidelity-matrix-v1.md) |
| 6b | Live resize + alt-screen + DECSTBM | **Done** | matrix rows F3–F5 |
| 6c | Grid selection + OSC 52 copy (US-2) | **Done** | host mouse drag; Ctrl+Shift+C |
| 6d | Crossterm event loop | **Done** | bounded PTY queues; keys/mouse/resize; final paint before EOF |
| 6e | Exact-head gates + ship `0.1.0` claim | **Done** | tag **`v0.1.0`** @ `60d23a3`; operator-b exact-head PASS; tickets closed |

## Post–Phase 1 polish (0.1.x)

| # | Item | Status | Notes |
|---|------|--------|--------|
| 6f | Clear error when run without a TTY | **Done** | `0b27ceb` |
| 6g | Click-only selection + visible host caret | **Done** | no sticky 1-cell inverse; `Show` after paint |
| 6h | OSC 52 no-op for whitespace-only extract | **Done** | blank drag does not fire clipboard |
| 6i | Architecture D9 (multi-core / GPU capacity) | **Done** | [decisions-v0.md](decisions-v0.md) D9; optional capacity, not a ship gate |
| 6j | Local Phase 1 test script | **Done** | `scripts/test-phase1.sh` |

## Classic 0.1.x dogfood pack — **shipped as 0.1.1**

| # | Item | Status | Notes |
|---|------|--------|--------|
| 12a | 256-color + truecolor SGR | **Done** | — matrix **F10** |
| 12b | Bracketed paste (DECSET 2004) | **Done** | — matrix **F11**; nested-wrap strip |
| 12c | Viewport keyboard selection + copy chords | **Done** | follow-ups — matrix **F12**; ADR-0001 |
| 12d | Host selection/clipboard policy (clean-room ADR) | **Done** | [ADR-0001](adr/0001-host-selection-clipboard.md) + decisions D10 |
| 12e | Matrix rows F10–F19 + claim bump | **Done** | `prismattyc-classic/0.1.1`; package `0.1.1` |
| 12f | Word / line multi-click | **Done** | double-click word; triple-click line |
| 12g | Multi-row visual EOL trim | **Done** | `selection_covers_cell` |
| 12h | Select-all + Home/End/Pg motion | **Done** | Ctrl+Shift+A; keyboard select motion |
| 12i | Hybrid mouse + wide/ZWJ + DECOM/DECAWM/1004 | **Done** | matrix **F13–F17** |
| 12j | Abs scrollback selection + extended/Kitty keys | **Done** | matrix **F18–F19**; ADR-0005 |

Further host UX **only** as tickets citing ADR gaps — implement original code from
the ADR, no vendored terminal sources.

## Phase 1.5 — Windowed host (own OS window)

Owner adoption gate: daily use without nesting inside Kitty/Ghostty.
See [ADR-0006](adr/0006-windowed-host.md). Nested `prism` remains the
`prismattyc-classic/*` claim harness.

| # | Item | Status | Notes |
|---|------|--------|--------|
| 13a | ADR-0006 + crate skeleton `prismattyc-host` | **Done** | winit + softbuffer + fontdue |
| 13b | MVP vertical slice (window, PTY, raster, keys, resize, quit) | **Done** | `cargo run -p prismattyc-host -- /bin/sh` |
| 13b2 | Nested Termwright E2E harness | **Done** | `./scripts/termwright-e2e.sh`; [termwright.md](termwright.md) |
| 13c | Host selection / clipboard on windowed path | **Done** | ADR-0001 parity; native arboard clipboard |
| 13d | Hybrid mouse + scrollback chrome on windowed path | **Done** | ADR-0003 / F18: wheel pan, Ctrl+Shift+arrows, app mouse when tracking |
| 13e | Windowed dogfood + optional matrix rows | **Done** | 2026-08-13. §5.6.1 steps 1–9 walked on windowed host + 2B control plane. No new F-row. Not A-6 PASS. Checklist: [13e-windowed-dogfood.md](13e-windowed-dogfood.md) |
| 13f | Styled underline raster support (PT-45) | **Done** | Windowed host SGR `4:x` / `58`; classic and mux/protocol passthrough remains deferred |
| 13g | Clipboard image paste hardening (PT-43) | **Done** | Windowed host accepts image data, one image file, or one `text/uri-list` image; generated PNGs keep the last 8 files |
| 13h | Background image + blur + tint (PT-47) | **Done** | PNG cover-scale, optional blur, tint toward theme bg; hot reload |
| 13i | Host accessibility spike (PT-34) | **Spike done** | [a11y-spike.md](a11y-spike.md), [ADR-0016](adr/0016-host-accessibility.md); adapter not implemented |

## Phase 2 — Multiplexer (authorized)

Owner authorized autonomous implementation through Phase 3 entry (2026-08-11).
Product intent: PRD §2.8 PASS @ `3896a3b`. Epic — implementation
done on main @ `3a923a9`; epic closeable after the dual-reviewed
wrap (2026-08-12). A-6 pack: [a6-evidence.md](a6-evidence.md) —
**published; formal A-6 PASS deferred** (0 eligible external operators;
owner dogfood first, cohort later). Phase 3 **entry opened 2026-08-13**
(owner directive) — see [ADR-0013](adr/0013-rich-surface-v1.md); A-6 stays
deferred and is not a Phase 3 gate (I-22).

| # | Item | Status | Notes |
|---|------|--------|-------|
| 14a | Domain model + IDs | **Done** |; PASS @ `f25000f` (N1–N6 + R1/R2); [ADR-0007](adr/0007-phase2-mux-domain.md) |
| 14b | Layout geometry | **Done** |; PASS @ `f25000f` — nested minima + atomic split/close |
| 14c | Multi-PTY `prismattyc-host` integration | **Done** | @ `740170f` — MuxRuntime + `--panes N`; 37 host tests |
| 14d | Control plane Unix socket v0 | **Done** | @ `58c320c` + harden `3adaedc`; [ADR-0008](adr/0008-control-plane-v0.md) |
| 14e | Controller lease per pane | **Done** | — acquire/release/takeover + WritePane observer reject |
| 14f | Host UX (tabs/chords/badges) | **Done** | @ `b710e1c` — chords + focus/unseen chrome; [ADR-0010](adr/0010-windowed-mux-chrome.md). Tabs T1–T4: [ADR-0012](adr/0012-host-tabs.md) |
| 14g | 2A proof harness | **Done** | @ `30789a3` — `./scripts/test-phase2a.sh` (3 host + 5 control); [proof matrix](phase2a-proof-harness.md); ADR-0009 |
| 14h | 2B long-lived server + attach | **Done** | @ `35d3e10` — `prismattyc-mux-server`/`attach`; `./scripts/test-phase2b-server.sh`; [ADR-0011](adr/0011-long-lived-mux-server.md) |
| 14i | 2B detach/reattach proof | **Done** | @ `abfcbcf` — `./scripts/test-phase2b-detach.sh`; [proof contract](phase2b-detach-proof.md) |
| 14j | A-6 evidence pack | **Pack published (A-6 PASS deferred)** | wrap 2026-08-12 @ `3a923a9`; cohort still 0; gate deferred, not pass/fail |
| 14k | Mux dogfood verbs | **Done** | @ `4f324c4` (#69). Stale-socket liveness + session create/switch + coalesced `output_activity` |
| 14l | Mux umbrella CLI | **T1 T2 T3** | `prismattyc-mux` up/attach/ls/new/status/stop/completions; [mux] config in [config.md](config.md). |

## Later

| # | Item | Status | Notes |
|---|------|--------|-------|
| 8 | Rich layer (styled runs v1; markup/animation/canvas unadvertised) | **Entry opened** | [ADR-0013](adr/0013-rich-surface-v1.md); strictly opt-in; capability-gated; §5.6 production checkpoint NOT claimed |
| 9b | Remote attach / multi-machine transport | First slice | [ssh-mux-attach-spike.md](ssh-mux-attach-spike.md). SSH TTY attach: display guard + runtime-dir hint. Windowed = Mac `prismattyc-host` + forwarded socket (later). Not Linux host over Wayland/X11. |
| 10 | Cross-platform polish | **Started** | Linux remains the claim. First slice: [macos.md](macos.md) + portable process/clipboard. |
| 11 | Further classic growth (grapheme/combining, …) | Optional | ADR-0004 through ZWJ/skin/RI cluster join; full UAX #29 still optional |

## Suggested execution order

1. Repo + skeleton + README (with charter) — **done**
2. Minimal viable classic terminal path + CI + Spike Baseline v0 — **Phase 0A landed on main**
3. Hybrid rendering freeze under real classic code pressure — **done**
4. Phase 0B experimental capability decoder + one bounded attachment — **done (PASS)**
5. Phase 1 first supported classic (`v0.1.0`) — **done**
6. Classic 0.1.x claim bump (`prismattyc-classic/0.1.1`, F1–F19) — **done**
7. Phase 1.5 windowed host (`prismattyc-host`) — **MVP done** (tip `14b4e8b`+)
8. Phase 2 PRD freeze (§2.8 v0.6) — **PASS** @ exact head **`3896a3b`** (operator-b; N1–N6 closed)
9. Phase 2 mux implementation — **done** on main @ `3a923a9`; epic closeable after the dual-reviewed wrap
10. wrap 2026-08-12: **pack published; A-6 PASS deferred** — owner dogfood, then external cohort
11. Rich layer Phase 3 **entry opened 2026-08-13** (ADR-0013); remote GPU still later; §5.6 production checkpoint still gated

## Tracking

- **Waypoint project:** `prism` (ticket prefix `PM`)
- **VectorVault team_id:** `prism`
- Large design outcomes should land in this tree and be cited from tickets, not only stored in chat or vault.
