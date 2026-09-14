# A-6 evidence pack — classic / mux operator value

**Status:** **Pack published / formal A-6 PASS still deferred.** Later cohort
work is (2026-08-12). Implementation + Termwright evidence refreshed;
eligible-operator count remains **0**. This document is the deliverable
plus the later-cohort log.
Tracks PRD hypothesis **A-6** and the frozen task set in
**[PRD.md §5.6.1](PRD.md)**.

- **Ticket:** (under epic) — wrap 2026-08-12.
- **Product freeze:** PRD §2.8 PASS @ `3896a3b`.
- **Implementation tip:** `origin/main` @ **`3a923a9`** (adopted-cell
  supervisor notice merged).
- **Owner authorization:** 2026-08-11 — autonomous Phase 2 implementation through
  Phase 3 entry authorized. **2026-08-11 (later same day):** owner has **no
  eligible external operators available now**; will recruit/run the cohort
  **after personal dogfood**. **2026-08-12:** owner asked operator-a + operator-b
  to wrap. Closing the tickets publishes this pack and the
  implementation epic; it does **not** record A-6 PASS or FAIL. External A-6
  remains evidence debt. Phase 3 is not opened.

## Gate language (from PRD)

A-6 gates **large** Phase 2 investment, not research, PRD freeze, or **bounded
prototypes**. Implementation proceeded under owner authorization as
bounded product build + proofs. Full cohort pass criteria (still required for
formal A-6 PASS):

- ≥5 eligible session operators
- ≥3 in one segment with recurring material pain
- ≥2 complete §5.6.1 with distinct value vs their external mux

Until the cohort is filled, record every run as **provisional** / pilot.
**Do not invent cohort completions.**

## Eligible operator exclusions

Prism implementers and PRD authors/reviewers do not count toward the external
cohort (PRD §1.6). Owner self-runs are recorded as **dogfood / pilot**, not
cohort completions. That is intentional and expected during the dogfood period.

## §5.6.1 task set (frozen)

| # | Task | 2A | 2B |
|---|------|----|----|
| 1 | Create three panes in one window | required | required |
| 2 | Discover focus + resize (one-page card max) | required | required |
| 3 | Distinct marker command per pane | required | required |
| 4 | Copy history/output from ≥1 pane | required | required |
| 5 | Close one pane; reflow; remaining PTYs intact | required | required |
| 6 | Close GUI / detach; processes stay alive | partial / N/A | required |
| 7 | Reattach; markers intact | N/A | required |
| 8 | Switch/create second session and return | required | required |
| 9 | Locate unseen output on background pane | required | required |

**Compare** same list on the operator’s current mux (tmux / Zellij / WezTerm /
Herdr / …). “Distinct value” = finished applicable steps **and** ≥1 material
advantage or equal capability with lower friction.

## Run log

| Date | Operator | Role | Segment | External mux | Prism head | Steps done | Wall-clock | Distinct value? | Notes |
|------|----------|------|---------|--------------|------------|------------|------------|-----------------|-------|
| 2026-08-11 | brandan (owner) | pilot / dogfood | Linux dev-tool sessions | Herdr + tmux | `ccfcd88`+ | deferred until 2A multi-pane | — | — | Authorization recorded; awaiting prototype |
| 2026-08-11 | — | prototype ready | — | — | `740170f` | unit: split/focus/resize/hit-test | — | n/a | multi-PTY landed; dogfood via `cargo run -p prismattyc-host -- --panes 3 -- /bin/sh`. Full §5.6.1 pilot still owner-run; interactive split chords. |
| 2026-08-11 | operator-b | implementation dogfood | Linux X11; 3–4 live `/bin/sh` PTYs | `prismattyc-host` | `feat/pm-38-host-ux` from `77714b0` | Alt+Left focus; split right/down; close/reflow | PASS | n/a | Header and OS title tracked pane/unseen counts; thick focus outline and amber background-output badges inspected in four OS-window captures. External cohort remains open. |
| 2026-08-11 | — | 2A proof harness | — | — | `30789a3` | unit: multi-PTY isolation, geometry, SIGWINCH, stale-ID, resync, backpressure, identity | PASS | n/a | `./scripts/test-phase2a.sh` on main. Multi-pane dogfood still `cargo run -p prismattyc-host -- --panes 3 -- /bin/sh` + chords; §5.6.1 external cohort open. |
| 2026-08-11 | — | 2B server/attach architecture | — | — | `35d3e10` | unit: disconnect preserves PTY; topology lockstep; idle attach timeouts | PASS | n/a | `./scripts/test-phase2b-server.sh`. Dogfood: `cargo run -p prismattyc-mux --bin prismattyc-mux-server -- -- /bin/sh` + `prism-mux-attach --write …`. Full §5.6.1 steps 6–7 await. |
| 2026-08-11 | operator-b | automated 2B durability proof | local same-user Unix socket | `prismattyc-mux-server` | `test/pm-41-detach-proof` from `ac72810` | §5.6.1 steps 6–7: detach with server/child alive; fresh attach sees intact markers and same pane/child | PASS | n/a | `./scripts/test-phase2b-detach.sh`; per-run JSON + summary retained under `e2e/artifacts/phase2b-detach/`. Implementation evidence only; external cohort remains open. |
| 2026-08-12 | operator-a | ticket wrap / pack publish | — | — | `3a923a9` | implementation proofs already on main; **no** new §5.6.1 operator completion | n/a | n/a | Prism lane merged (PR #64). Owner is daily-using `prismattyc-host`; that is dogfood context, **not** a scored §5.6.1 row. Dual-reviewed wrap with operator-b. |
| 2026-08-12 | operator-a | later-cohort implementation re-run | — | nested Termwright + 2A/2B scripts | `b3a384c` | Termwright classic-shell/color/keys **PASS**; new `a6-nested-marker` **PASS** (`echo A6PM43MARKER`); `test-phase2a.sh` **PASS**; `test-phase2b-server.sh` **PASS**; `test-phase2b-detach.sh` **PASS** (markers `PM41-ONE/TWO-20260812T210611Z-2173066`, same child after reattach) | ~9s + marker scenario | n/a | **Not a cohort completion.** Termwright does not drive `prismattyc-host`. Artifacts: `e2e/artifacts/20260812-150605/`, `e2e/artifacts/20260812-150801/`, `e2e/artifacts/phase2b-detach/20260812T210611Z-2173066/`. Operator card: [a6-operator-card.md](a6-operator-card.md). |

## Pilot run paths (implementation ready @ `3a923a9`)

**2A multi-pane (windowed host):**

```bash
cargo run -p prismattyc-host -- --panes 3 -- /bin/sh
# chords (ADR-0010): Ctrl+Shift+\ split-right; Ctrl+Shift+- split-down;
# Ctrl+Shift+W close; Alt+Arrow focus; reflow on close
```

**2B detach/reattach (long-lived server):**

```bash
./scripts/test-phase2b-detach.sh   # automated durability proof
# or interactive:
cargo run -p prismattyc-mux --bin prismattyc-mux-server -- -- /bin/sh
cargo run -p prismattyc-mux --bin prism-mux-attach -- --write $'marker\n'
# kill attach; re-run attach and confirm marker + child_alive
```

Owner/external operators record full §5.6.1 completions in the run log above.
Agents must not invent cohort completions.

## External mux comparison notes

Fill after each §5.6.1 run. Template:

- **Misroutes:** …
- **Recovery steps:** …
- **Would drop external mux for this workflow?** yes / no / partial — why
- **Material advantages claimed:** …

## Cohort status

| Metric | Target | Current |
|--------|--------|---------|
| Eligible operators interviewed | 5 | **0 cohort** (implementers/reviewers excluded — PRD §1.6) |
| Segment cluster (≥3 material pain) | 1 segment | none (no eligible interviews) |
| Completions with distinct value | 2 | 0 cohort |
| Gate | pass / fail / inconclusive / deferred | **deferred — later cohort opened as formal PASS not claimed** |

## Decision log

| Date | Decision | By |
|------|----------|-----|
| 2026-08-11 | Owner authorizes Phase 2 implementation tickets and autonomous progress through Phase 3 entry. A-6 cohort remains open; bounded 2A/2B prototype is the vehicle for §5.6.1 once multi-pane exists. | owner → operator-a |
| 2026-08-11 | multi-PTY host on main @ `740170f` (`--panes N`, per-pane SIGWINCH). A-6 pilot §5.6.1 runs unblocked for owner dogfood; external cohort still open. | operator-a |
| 2026-08-11 | 2A proof harness on main @ `30789a3` (`scripts/test-phase2a.sh`, ADR-0009 connection-bound leases). Deterministic §2.8.12 model/control/offscreen gates green; 2B detach still open (41). External A-6 cohort open. | operator-a |
| 2026-08-11 | long-lived server + attach on main @ `35d3e10` (`prismattyc-mux-server`/`prism-mux-attach`, ADR-0011, `scripts/test-phase2b-server.sh`). Client disconnect preserves PTYs owns full operator detach/reattach dogfood. External A-6 cohort open. | operator-a |
| 2026-08-11 | detach/reattach durability on main @ `abfcbcf` (`scripts/test-phase2b-detach.sh`). §2.8.12 2B process-alive proof green (implementation). Phase 2 mux implementation tickets complete; external A-6 cohort remains open. Phase 3 rich still gated. | operator-a |
| 2026-08-11 | **No eligible external operators available now.** Owner will dogfood Prism mux personally for a while, then recruit/run the A-6 cohort. stays open as **deferred** (not PASS). Owner pilot rows welcome in the run log; they do not fill the ≥5 cohort bar. Phase 3 not auto-started. | owner → operator-a |
| 2026-08-12 | **Wrap.** Evidence pack is the ticket deliverable. Formal A-6 PASS criteria in PRD §5.6 are unchanged and unmet (0 eligible external operators). Ticket close = pack published + implementation epic complete, **not** A-6 PASS. Phase 3 remains gated. Dual review: operator-a authors this file; operator-b reviews; inverse on roadmap/PRD. | owner → operator-a + operator-b |
| 2026-08-12 | **Later A-6 cohort.** Owner asked operator-a + operator-b to complete the later cohort and use Termwright where useful. §1.6 exclusions unchanged: we are not eligible operators. Work package = operator card, Termwright nested (incl. `a6-nested-marker`), re-run 2A/2B/detach harnesses at `b3a384c`, live pilot if operator-b can run §5.6.1 on `prismattyc-host`. Formal PASS still requires five eligible humans. | owner → operator-a + operator-b |

## Related

- [PRD.md §2.8](PRD.md) — product freeze (intent). A-6 PASS criteria: §5.6 / §5.6.1
- [PRD-phase2-mux-outline.md](PRD-phase2-mux-outline.md) — ticket budget map
- [mux-research.md](mux-research.md) — competitive notes
- [roadmap.md](roadmap.md) — done; pack is 14j; later cohort is
- [a6-operator-card.md](a6-operator-card.md) — one-page §5.6.1 discoverability card
- [termwright.md](termwright.md) — nested E2E only; not `prismattyc-host`
- Prism [PR #64](https://github.com/brandanmajeske/Prism/pull/64) — `supervisor.cell_exited`
- Epic children (implementation), (pack wrap), (later cohort)
