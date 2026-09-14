# Architecture decisions v0 (MVP freeze)

**Status:** Proposed freeze for Phase 0. Open to operator-b dissent on Crosstalk; treat as working agreement until superseded.
**Parent epic:**
**Ticket:**
**Sources:** [Prismattyc-Charter.md](../Prismattyc-Charter.md), [architecture.md](architecture.md), [hybrid-rendering.md](hybrid-rendering.md), [capability-protocol.md](capability-protocol.md), [workspace.md](workspace.md)

These are **product/engineering leans for the first build**, not eternal law. Change them by updating this file and commenting on.

---

## D1 — Classic path is the hard gate

**Decision:** Every existing modern-terminal workload must keep working. Regressions in VT behavior, PTY lifecycle, scrollback, or input are P0 over rich features.

**Why:** Charter principle 1.

**Implication:** Tests and CI prioritize classic path. Rich features must degrade or disable without breaking classic.

---

## D2 — Single-process host for MVP

**Decision (Phase 0/1 historical):** First useful Prismattyc is one process: PTY + parser + screen + render + input in-process.

**Phase 2 supersedure:** Product intent for detach durability and control plane is frozen in **[PRD.md §2.8](PRD.md)** (v0.6+). **2A** may remain in-process composition; **2B** requires a long-lived server + attachable client for detach. **Remote** multi-machine transport remains Later. Do not read this D2 as forbidding Phase 2B.

**Why (historical):** Reduced surface area while proving classic fidelity; matched starting-points order for Phase 0/1.

**Implication (historical):** Skeleton binary `prism` owned the event loop were not blocked on a daemon protocol.

---

## D3 — Software / terminal-backed render first

**Decision:** Default backend is software (and/or rendering into a host terminal for early dev). GPU / Wayland / X11 are **feature-flagged later**, not required for MVP classic path.

**Why:** Correctness and latency of the cell grid matter before GPU integration cost.

**Implication:** `prismattyc-render` exposes a backend trait or equivalent; no hard GPU dependency in default features.

---

## D4 — Crate boundaries (workspace.md)

**Decision:** Workspace crates:

| Crate | Owns |
|-------|------|
| `prism` | Binary / composition root |
| `prismattyc-core` | Screen model, shared types, errors |
| `prismattyc-emulator` | VT parse + PTY I/O |
| `prismattyc-mux` | Sessions/windows/panes (stub OK until post-classic) |
| `prismattyc-render` | Classic grid draw + hooks for rich |
| `prismattyc-protocol` | Capability + future rich protocol types only |

**Rules:**

- `prismattyc-emulator` **must not** depend on a rich document/DOM type graph.
- `prismattyc-protocol` stays dependency-light so tools/tests can use it alone.
- Mux may stay thin stubs until classic path is solid.

---

## D5 — Hybrid rendering lean (deepened by)

**Decision (v0 freeze in [hybrid-rendering.md](hybrid-rendering.md)):**

1. Classic cell grid is always present and authoritative for unaware apps.
2. Rich content is opt-in and **attached** (not a free-floating OS window).
3. First anchor kinds: **cell-rect** and **viewport overlay** (HUD).
4. Empty rich layer cost ≈ one null check (no animation clocks when unused).
5. **One primary input caret** per pane at a time.
6. Default selection is grid-native; rich hit-testing only when a region claims it.
7. Mouse: rich claims only if it accepts hits; else classic mouse protocols.
8. **Z-order** (primary): grid → cell-rect → viewport overlay → selection → caret → host chrome (topmost); app content clipped to pane content rect.
9. **Alt-screen:** no scrollback; **suspend/preserve** primary rich state (no paint/hit-test/update while alt active); alt-scoped IDs separate; destroy alt attachments on exit then resume primary. Not a scrollback snapshot.
10. **Scrollback:** live rich trees do not persist in history; translate+clip while any of a cell-rect remains visible; detach only when fully outside; frozen snapshots deferred. Clip never suppresses classic cells.
11. **A11y v0:** plain grid text remains the guaranteed textual surface; rich a11y tree later. Host chrome and the focused-pane document node are [ADR-0016](adr/0016-host-accessibility.md).

**Still deferred:** wire paint opcodes, line-sticky anchors, full-pane takeover,
multi-format clipboard, numeric paint SLOs.

**Protocol 0.3 amendment:** [ADR-0014](adr/0014-rich-surface-v2-fabric.md)
adds one top reserved-row workspace that shrinks the guest grid. It does not
change the two `0.1`/`0.2` attachment kinds or authorize full-pane takeover.

---

## D6 — Capability / rich protocol posture

**Decision:**

1. **Query before emit** for any Prismattyc-specific rich feature.
2. Classic VT/xterm (and later Sixel/Kitty if implemented) remain ungated classic-path extensions.
3. **Wire encoding first-cut hypothesis:** bounded **APC** with case-sensitive
   `Prismattyc;` namespace (see [capability-protocol.md](capability-protocol.md)), designed
   to be ignored by hosts that do not implement this bounded namespace; safe behavior
   across declared hosts/middleboxes remains gated by the **transport matrix** (not a
   claim that all foreign hosts ignore APC). Semantic capability/feature types are
   **on main**. Full encoder/decoder paths and rich command traffic remain **deferred**
   and subject to transport evidence before production freeze. Phase 3 entry
   scope for wire v1 is [ADR-0013](adr/0013-rich-surface-v1.md) (2026-08-13);
   the `0.3` rich-v2 cut is [ADR-0014](adr/0014-rich-surface-v2-fabric.md);
   the production freeze itself stays gated on the transport matrix and PRD
   §5.6.
4. OSC/DCS alternatives are not the reviewable first cut; reopen only with evidence
   that APC is insufficient for the capability family.

---

## D7 — Platform and toolchain

| Item | Decision |
|------|----------|
| Language | Rust |
| Edition | **2021** for skeleton (revisit 2024 when MSRV allows and team agrees) |
| MSRV | Document a concrete recent stable in README when skeleton lands (implementer picks; note in PR) |
| OS | Linux primary |
| License | **TBD** (owner); do not invent LICENSE without Brandan |

---

## D8 — Parallel work split (Phase 0)

| Lane | Owner |
|------|-------|
| Build skeleton → CI → classic path | **operator-b** |
| Architecture / hybrid decision freeze | **operator-a** |
| PM / board | **operator-a** |

**Coordination:** separate branches/worktrees if both touch code; operator-a stays on `docs/` unless explicitly pairing. Signal on Crosstalk; durable outcomes in git + Waypoint.

---

## D9 — Multi-core concurrency and GPU capacity (planned use)

**Decision:** Prismattyc will **plan to use** multi-core CPUs and GPU-accelerated graphics where they improve latency, throughput, or rich paint quality — as **optional capacity**, not as requirements for the classic path. Neither is a Phase 0/1 ship gate.

### Multi-core / multi-threading

1. **Now (classic host):** Prefer **I/O isolation threads** so PTY read/write cannot stall host input or paint. Bounded queues and single-writer rules stay the correctness model (Phase 1 already uses reader + writer threads + main loop).
2. **Heavy classic work** (VT parse → screen model → paint) may remain **serialized on the host loop** until measurements show a real bottleneck. Full-grid re-render and single-threaded parse are acceptable until latency/CPU baselines exist (see PRD measurement categories).
3. **Later (mux and scale):** When multi-pane / heavy work lands, use multi-core at clear seams — e.g. per-pane I/O or parse, dirty-region/damage paint tiles, scrollback search, large paste — without blurring **data ownership** (screen model per pane, single writer to child stdin).
4. **Do not require** many cores or a worker pool for basic `prism` on a single pane. Prefer simple threading until a general job system is justified by data.
5. **No prescribed framework yet** (raw threads, `rayon`, async runtime, etc.). Choose when implementing a concrete seam; document the choice then.

### GPU / native display backends

1. **Deepens D3:** default remains software and/or host-terminal-backed. GPU / Wayland / X11 stay **feature-flagged later** production backends.
2. **`prismattyc-render` stays backend-swappable** (trait or feature-gated modules). No hard GPU dependency in default features; classic fallback must keep working when GPU is unavailable or disabled.
3. **API choice open** (e.g. wgpu vs platform-native). Freeze only when a production backend ticket lands.
4. GPU must not force rich adoption or break unaware classic apps (D1, D5).

### Non-goals of this decision

- Requiring GPU or multi-core for MVP / Phase 1 classic support.
- Inventing numeric latency or CPU SLOs before baselines (PRD T-5).
- Replacing D2 (single-process MVP) with a client/server split — that remains roadmap-later.

**Why:** Modern hosts often have spare cores and GPUs; designs should not paint Prismattyc into a single-core, CPU-only corner. Classic correctness and software-first ship still outrank premature parallelism and GPU integration cost.

**Implication:** Architecture sketches and later ADRs treat multi-core and GPU as **capacity we intend to use**, gated by classic fidelity, hybrid rules, and measurement. Update this decision when the first worker-pool or GPU backend lands.

**Related:** D2 (single-process MVP), D3 (software-first render), [architecture.md](architecture.md) open question on GPU timeline, [workspace.md](workspace.md) feature flags / swappable backends, PRD later items (GPU backends; latency/CPU baselines).

---

## D10 — Host selection / clipboard clean-room policy

**Decision:** Classic **host** selection, copy/paste chords, and key ownership are frozen in
[adr/0001-host-selection-clipboard.md](adr/0001-host-selection-clipboard.md). Implementation is
**original Prismattyc code** only: study public behavior and wire specs, write policy in-repo,
implement from the ADR. **No** vendoring or copy-paste of other terminal emulators’ sources
(especially no GPL implementation without a separate product decision).

**Why:** Dogfood showed ad-hoc host UX thrash and outer-host chord theft; clean-room keeps
license risk low while matching user expectations.

**Implication:** New chords, clear rules, or selection geometry changes update ADR-0001 (or
supersede it); tickets cite D-H section IDs.

### D11 — Host mouse: selection only (classic 0.1.x) → hybrid (ADR-0003)

**Decision (original):** Classic claim is **host selection / scroll only**. Application
mouse reporting was an intentional exclusion — [ADR-0002](adr/0002-host-mouse-policy.md).

**Update:** [ADR-0003](adr/0003-hybrid-mouse.md) implements **hybrid** (Option B): plain
mouse → app SGR/X10 when the child enables 1000/1002/1003; **Shift** keeps host
selection. Closes as implementation.

**Why hybrid:** Dual ownership needs an explicit rule; Shift is the standard escape
hatch; vim/htop become usable without abandoning host select.

---

## Explicit non-goals for Phase 0

- Full multiplexer productization
- Rich markup/canvas UX polish
- Browser/Electron paths
- Competing as an AI-first terminal product
- Final capability wire format

---

## Acceptance for

- [x] This file exists and states D1–D8
- [x] D9 records multi-core concurrency + GPU capacity posture (post–Phase 1 architectural lean)
- [ ] operator-b ack (or recorded dissent) on Crosstalk
- [ ] Linked from [docs/README.md](README.md) and [roadmap.md](roadmap.md)
- Ticket closed with outcome pointing at commit

---

## Change log

| Date | Change |
|------|--------|
| 2026-07-28 | Initial freeze draft by operator-a for concurrent work with operator-b |
| 2026-07-29 | **D9** — multi-core concurrency and GPU capacity as planned optional use; does not change D3 software-first or Phase 1 gates |
| 2026-07-29 | **D10** — host selection/clipboard clean-room ADR-0001 |
| 2026-07-31 | **D11** — host mouse selection-only (ADR-0002 policy) |
| 2026-07-31 | **D11 update** — hybrid mouse (ADR-0003 Option B); supersedes D-M1 claim |
