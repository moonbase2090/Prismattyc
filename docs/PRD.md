# Prism — Product Requirements Document (PRD)

| Field | Value |
|-------|--------|
| **Product** | Prism |
| **Tagline** | Classic terminal. Modern surface. |
| **Status** | v0.6 — Phase 2 mux product intent **frozen** (operator-b PRD-REVIEW **PASS** @ `3896a3b`) |
| **Authors** | operator-a (PM/synthesis), operator-b (v0.5 product-validation + Phase 2 PRD-BRAINSTORM + PRD-REVIEW) |
| **Date** | 2026-08-11 |
| **Sources** | Charter, starting points, decisions-v0, architecture/hybrid docs, operator-b v0.5 brainstorm, Phase 1.5 / ADR-0006, [mux-research.md](mux-research.md), operator-b Phase 2 brainstorm `01KZRJBV03A56QK3BKFPZCB5PF`, PRD-REVIEW PASS `01KZRRXDKG8FTMCNJ4FTCZPJAZ` |
| **Tracking** | Waypoint project `prism` (prefix `PM`); VectorVault `team_id=prism` / `task_id=prism-prd` |
| **Pre-Mux tip** | Tag **`Pre-Mux`** + Phase 1.5 tip **`14b4e8b`** (verify with `git rev-parse origin/main`) |
| **Freeze SHA** | **`3896a3b69cee2fb54377fadf47f534317d09b233`** — material product changes require a new exact-head review |

**How this PRD was produced:** Owner requested a structured PRD written with Codex on a live Crosstalk wire. operator-a proposed the section spine; operator-b returned a full `PRD-BRAINSTORM` and iterative `PRD-REVIEW` cycles. v0.5 adds product-validation gates; closed under adversarial review (R1–R11, N1–N3). **v0.6** freezes Phase 2 multiplexer product intent from [mux-research.md](mux-research.md) and operator-b Night Shift brainstorm; operator-b adversarial PASS @ exact head `3896a3b` (N1–N6 closed). **Not** implementation authorization — A-6 still gates large mux investment; owner must authorize before filing implementation tickets. Git path `docs/PRD.md` is the source of truth.

### Spaces positioning amendment

**Spaces positioning amendment — Brandan, 2026-09-10:** Market Spaces as
an agent-team workspace. This updates earlier positioning language and
permits Space seat presentation and native mail transport. I-5 remains:
Prismattyc is not an agent orchestration runtime. Agent CLIs reason and
choose work. Classic fidelity and product-validation gates remain in
force. See the [Spaces decision and rollback baseline](design/spaces-agent-centric-prd.md#positioning-decision-and-rollback-baseline).

### State labels (read before claiming “done”)

| Label | Meaning |
|-------|---------|
| **On main** | Merged to `origin/main` |
| **In review** | Implemented on a review branch/worktree at a frozen SHA; not yet merged |
| **Planned** | Required or intended; not implemented |

Do **not** claim that capability messages are encoded/decoded on the host, or that the first classic vertical slice is “charter-complete modern terminal,” unless the state table says so.

---

## 1. Problem statement

### 1.1 Context

The terminal remains the most reliable, scriptable, and efficient place for text-oriented work: shells, compilers, remote sessions, TUI tools, and agent CLIs. That strength depends on a **cell-grid emulation contract** (VT/xterm and extensions) that decades of software already assume.

### 1.2 Pain

Developers lack **one coherent product contract** that combines:

1. **High-fidelity classic terminal behavior**,
2. **First-class multiplexing**, and
3. A **queryable, opt-in rich surface** with **explicit cursor, selection, and input semantics**.

Existing pieces do not provide one interoperable contract spanning classic emulation, built-in mux, capability negotiation, and hybrid interaction rules; app authors must target host-specific capabilities and own fallback behavior.

Secondary pains:

- **Discoverability failure** — apps cannot reliably know whether optional richness exists.
- **Hybrid conflict** — bolted-on modern layers fight the grid over caret, selection, mouse, and damage.
- **Mux as afterthought** — power users still need sessions/panes/detach without sacrificing fidelity.

### 1.3 Opportunity

Ship a **native** terminal host where **classic fidelity is a release gate** (not merely a priority), and richness is strictly opt-in behind a versioned capability protocol.

The gate has the two bounded meanings in I-20: Phase 0A has an internal subset
exit, while a product release requires the published fidelity matrix to be green
for the claimed version. Neither means universal terminal parity.

### 1.4 Success looks like

- Unaware apps experience a high-quality classic terminal.
- Aware apps detect features, use negotiated versions, and degrade cleanly.
- Operators get multiplexing without redefining PTY ownership.
- Rich work **cannot redefine or bypass** classic behavior.
- No browser engine; Prism is not an IDE and not an agent runtime.

### 1.5 Product assumptions requiring validation

The architecture is deliberate; the market and adoption claims are still
**hypotheses**. They must not be rewritten as established facts without linked
evidence.

| ID | Hypothesis | Evidence required |
|----|------------|-------------------|
| A-1 | A recruitable application-author segment has recurring, documented pain between the cell grid and a browser application shell | Discovery interviews with concrete current applications, workarounds, frequency, and costs |
| A-2 | Those authors will adopt a queryable rich surface while maintaining a useful classic fallback | Independently built prototype clients and fallback observation on non-Prism hosts |
| A-3 | Capability discovery remains usable through the transports developers actually use | Direct, SSH, tmux, and nested-middlebox conformance results |
| A-4 | Prism can bound classic compatibility tightly enough to ship incremental value | A published supported-fidelity matrix with named references and explicit exclusions |
| A-5 | The differentiating rich workflow is valuable enough to justify a production rich-surface investment | A narrow end-to-end rich experiment evaluated by target authors before large Phase 3 investment |
| A-6 | A classic terminal and multiplexer has independently validated value before large Phase 2 investment, regardless of the rich result | Separate classic/mux discovery and workflow evidence before large Phase 2 investment |

### 1.6 Candidate initial wedge and discovery gate

The candidate initial adopter is a **Linux-first developer-tool or TUI author**
who needs a small amount of structured presentation (for example, a diagram,
status surface, or interactive visual region) but must retain a terminal-native
workflow and a usable text fallback over SSH. This is a recruitment hypothesis,
not a committed ideal-customer profile.

The counts below are **initial minimums for an evidence checkpoint**, not a
statistical market-size claim. Revise them when recruitment reveals multiple
segments or the evidence is too heterogeneous to support one conclusion.

An **eligible non-core application author** must have independently shipped or
maintained a terminal-facing application used by someone beyond themselves
during the prior 12 months, before being recruited for Prism discovery. Prism
implementers and PRD authors/reviewers do not count. Record recruitment channel
and prior relationship so convenience-sample bias remains visible.

An **eligible session operator** must use a terminal multiplexer or manage
multiple persistent terminal sessions at least weekly, and must meet the same
Prism-independence exclusions.

Before committing to full rich-surface productization, the team must:

1. Interview at least **five** eligible non-core application authors; at least **three in the same candidate segment** must provide a concrete application, recurring task, current workaround, and material cost or abandoned capability.
2. Have at least **two** eligible non-core authors build or adapt prototype clients from public documentation without modifying Prism internals.
3. Observe both negotiated-rich and classic-fallback tasks with eligible target authors; record task outcome, failure modes, preference, and integration effort rather than accepting enthusiasm alone.
4. Publish an alternatives brief covering relevant terminal extension/graphics approaches, standalone GUI/TUI frameworks, and browser-backed shells. It must explain the interoperability gap Prism fills without dismissing alternatives as universally weak or ad hoc.

The pass/fail/inconclusive rubric is in §5.6. Failure stops or narrows the
**rich-surface** investment. Classic emulator/mux work may proceed only if A-6
passes its separate evidence gate; it is not automatically approved or killed
by the rich result. Neither outcome justifies lowering the classic fidelity bar.

---

## 2. Solution description

### 2.1 Product summary

**Prism** is a native emulator and multiplexer distributed as a normal host binary:

1. **Classic path (always on, release gate):** PTY lifecycle, VT/xterm parsing (expanding coverage under a fidelity matrix), cell grid + bounded scrollback, input, efficient render.
2. **Multiplexer (first-class product goal):** sessions, windows, panes, layouts, detach/reattach, sharing—after classic is solid enough to protect.
3. **Opt-in rich path:** markup, styling, animation, canvas—only after **capability query/reply**, composed in a **hybrid model** so layers do not fight over cursor, selection, or input.

**Personas:**

| ID | Persona |
|----|---------|
| P1 | CLI power user / developer |
| P2 | TUI / rich application author |
| P3 | Session / multiplexer operator |
| P4 | Contributor / maintainer |

Fabric/agent operation is **secondary**: Prism is a terminal surface that can host agent CLIs; it is **not** an agent orchestration runtime (Hive/Crosstalk/Waypoint stay separate).

### 2.2 Layered design concept

| Layer | Role |
|-------|------|
| PTY / process | Spawn child; **single writer** ownership of child stdin |
| Emulator | Parse sequences; maintain classic screen model |
| Screen model | Cells + scrollback; optional rich attachments later |
| Renderer | Classic path always; rich path only when content exists |
| Protocol | Capability discovery + future rich commands (`prismattyc-protocol`) |
| Mux | Layout and session control (product phase after classic gate) |
| Host binary | Composition root (`prism`) |

### 2.3 Platform

- Rust workspace; **edition 2021**; **MSRV 1.90**
- Linux primary; cross-platform later
- Normal host binary (no Electron / required browser runtime)

### 2.4 Capability posture (rich opt-in)

First-cut design (**in review** on stack unless noted otherwise on main):

The APC envelope is a **provisional design candidate** pending the §5.6
transport matrix. does not freeze a production wire protocol.

- Classic VT is **never** gated on Prism capabilities.
- Affirmative capability reply required before any Prism-specific rich command.
- Envelope: **7-bit APC** `ESC _` … `ESC \`, namespace `Prismattyc;`, printable ASCII body **≤ 4096** bytes, nonzero correlation ids, major/minor version negotiation, explicit feature registry.
- No reply / malformed / timeout ⇒ **classic-only**.
- Parsing bounded; must not stall PTY processing; must not blank/freeze the grid.
- **tmux/middlebox pass-through is not assumed**; classic-only is correct when APC is swallowed.
- Encoder/decoder and rich **emission** may land after design; do not claim live encode/decode until implemented on main.

See [capability-protocol.md](capability-protocol.md) for wire detail when merged.

### 2.5 Hybrid rendering posture

Classic grid authoritative for unaware apps. Rich content **attached** (cell-rect + viewport overlay first). Empty rich layer: **no active animation clock** and no meaningful idle-path work. One primary caret per pane. Default selection grid-native.

### 2.6 Phasing

| Phase | Intent | Fidelity note |
|-------|--------|----------------|
| **Phase 0A — internal slice** | Workspace, CI, first vertical classic slice (PTY → VT *subset* → grid/scrollback → render → input); capability **design/types** | Internal implementation milestone, **not a supported release**; missing fidelity remains explicit |
| **Phase 0B — internal validation spike** | After the Spike Baseline v0 and entry criteria below pass, prove capability query/reply plus **one** bounded rich attachment and a required classic textual fallback | Internal/limited research build, not a production protocol or compatibility claim; complete before large Phase 3 investment |
| **Phase 1 — first supported classic release** | Grow and publish the VT/fidelity matrix to a named release subset; add **basic grid selection/copy** (US-2) | Product release gate: every required case is green for the claimed version; required before external “supported terminal” or broad classic-parity claims |
| **Phase 1.5** | Windowed OS host (`prismattyc-host`) | Own window; nested `prism` remains classic claim / Termwright harness |
| **Phase 2** | Multiplexer productization (**2A** in-process composition + **2B** detach durability) | Detach/reattach is part of the **product claim** (charter principle 5); 2A alone is **not** “mux complete” — see **§2.8** |
| **Phase 3** | Rich commands + hybrid enforcement in code | Behind capability gates |
| **Later** | Remote transport, cold resurrection, GPU backends as features | Not Phase 0 blockers; cold restore ≠ detach |

#### Phase 0A exit, Phase 0B entry, and allowed limitations

Phase 0A exits only when the published **Spike Baseline v0** has no open P0
defect and all of its enumerated fixtures pass at a frozen head. That is an
internal subset exit, not the classic product release gate. The product release
gate is the larger, versioned Phase 1 fidelity matrix green for the claimed
release.

Phase 0B may then precede Phase 1 only as an explicitly experimental validation
build. Spike Baseline v0 covers:

- real PTY spawn, input, child exit, and host-terminal restoration;
- single-column printable text (ASCII required), CR/LF/backspace/tab;
- the documented cursor, erase, and basic 16-color SGR subset;
- delayed autowrap, bounded scrollback, deterministic render; and
- containment of unsupported control strings without payload leakage, hang,
  crash, or grid corruption.

Its explicit missing-feature allowlist is: child alternate-screen behavior,
mouse reporting, live SIGWINCH/resize, wide Unicode/grapheme correctness,
bracketed paste, and extended keyboard protocols. A spike application must not
depend on those features. No demo, screenshot, or user study may describe this
baseline as modern-terminal compatible.

Before any non-core author integration, Phase 0B also requires:

1. an experimental host flag that is off by default;
2. a bounded capability-query decoder and reply encoder;
3. one bounded attachment type with explicit cursor/selection/input behavior;
4. a classic textual fallback exercising the same user task;
5. public spike documentation with version, limits, timeout, and fallback; and
6. an end-to-end safety harness covering the Spike Baseline v0.

A client or harness built by the Prism implementation team may dogfood the
spike, but does **not** satisfy the independent-integration gate.

### 2.7 Current implementation state / evidence

| Item | State | Evidence notes |
|------|--------|----------------|
| Cargo workspace (six crates, MSRV 1.90) | **On main** | `4540d22` |
| PRD v0.4 baseline | **On main** | `6e473de`; superseded by v0.5 |
| PRD v0.5 | **On main** | Product-gate content at `d117fd1` (N2/N3 correction lineage: `f993eaa`, `8fed7c8`) |
| CI workflow (fmt/check/test/clippy locked) | **On main** | landed as `5e8a951`; live GHA green run `30417135305` @ `63dbac8` |
| Capability first cut | **On main** | landed as `b5d17d3`: semantic types + design docs; **no host decoder/emitter of APC traffic yet** |
| Classic PTY vertical slice | **On main** | landed as `e56d868` stack tip: real PTY, vte subset, 10k scrollback, ANSI render, raw input; **16 tests** re-verified on main; **not** charter-complete fidelity (no child alt-screen model, mouse, live SIGWINCH as complete features) |
| Spike Baseline v0 | **Published** | [spike-baseline-v0.md](spike-baseline-v0.md); frozen implementation head `e56d868` |
| Hybrid composition freeze | **On main** | @ `63dbac8` |
| Phase 0B experimental spike | **Closed (PASS)** | @ `534284b`; CI `30418912701`; [phase-0b-spike.md](phase-0b-spike.md) — experimental only, not a product release |
| Phase 1 classic claim `prism-classic/0.1.0` | **Shipped** | Tag **`v0.1.0`** @ `60d23a3`; matrix [fidelity-matrix-v1.md](fidelity-matrix-v1.md) done; operator-b exact-head PASS |
| Post-1 polish (TTY bail, selection/caret, blank OSC 52, D9) | **On main** | `0b27ceb` TTY message selection/caret; whitespace-only OSC 52 no-op; [decisions-v0.md](decisions-v0.md) D9; `scripts/test-phase1.sh` |
| Classic claim `prismattyc-classic/0.1.1` (F1–F19) | **Shipped** | Same matrix as the former `prism-classic/0.1.1` id; package `0.1.1`; hybrid mouse, wide/ZWJ, DECOM/DECAWM/1004, abs scrollback select, Kitty CSI-u |
| Phase 1.5 windowed host (`prismattyc-host`) | **On main** (MVP) | ADR-0006; tip **`14b4e8b`**; Termwright nested E2E; native selection/clipboard + scrollback/hybrid mouse |
| Phase 2 mux product | **Implemented** (A-6 PASS deferred) | §2.8 PASS @ **`3896a3b`**; implementation on main @ **`3a923a9`**; wrap 2026-08-12: evidence pack **published**, external cohort still 0, gate **deferred** (not pass/fail) — [a6-evidence.md](a6-evidence.md) |
| Rich paint / GPU backends | **Planned** | Phase 3 / Later; D3+D9 |

Requirements below describe **product intent**. Only rows marked **On main** / **Shipped** are shipped facts. Always `git rev-parse origin/main` before status claims.

### 2.8 Phase 2 multiplexer product freeze (v0.6)

**Status:** **Frozen** — operator-b PRD-REVIEW **PASS** at exact head **`3896a3b`** (N1–N6 closed). Freezes **product intent**, not ticket filing or implementation authorization.
**Research:** [mux-research.md](mux-research.md). **Outline:** [PRD-phase2-mux-outline.md](PRD-phase2-mux-outline.md).

#### 2.8.1 Scope and ship language

| Slice | Ships | May **not** be called |
|-------|--------|------------------------|
| **2A — In-process composition** | Sessions, windows (tabs), pane splits, focus, layout, per-pane PTY+emulator on **`prismattyc-host`**, control plane API | “Mux complete,” “charter mux done,” or “detach product” |
| **2B — Detach durability** | Long-lived server, client attach/detach, live PTYs continue after GUI exit | Optional remote transport (Later) |

**Detach/reattach is mandatory for the overall Phase 2 product claim** (charter principle 5; §2.1). Sequencing 2A then 2B is allowed; claiming Phase 2 done after 2A alone is not. **Sharing** in Phase 2 is local multi-client: **read-only observers** plus **at most one writable controller lease per pane** (clients may hold leases on distinct panes). **Remote** multi-machine transport is Later.

**A-6** gates **large implementation investment**, not research, PRD freeze, or bounded prototypes. The frozen A-6 operator task set is in **§5.6.1**.

#### 2.8.2 Information architecture

```
Domain / server
  └── Session (named)
        └── Window (tab)
              └── PaneLayout (immutable binary split tree)
                    └── Pane → PtySession + Emulator + Screen
```

- Labels/indexes are presentation only; control targets use **opaque stable typed IDs** never reused for the server lifetime (tmux/Herdr lesson).
- Each pane owns **exactly one** PTY + emulator + screen (one VT brain per pane).

#### 2.8.3 State ownership

| Owner | Owns |
|-------|------|
| **Server** | PTYs, emulators, topology, authoritative terminal/cell geometry |
| **Each attached GUI client** | Focus, viewport scroll, selection/copy, hover, transient zoom |

**Zoom** is a **view projection**, never a topology mutation.

#### 2.8.4 Multi-client policy

**Freeze (product):** controller lease grain is **per pane**.

- **At most one writable controller lease per pane.** A single client may hold leases for multiple panes; two clients must not both write the same pane.
- Clients without the lease for a pane are **read-only observers** for that pane until **explicit takeover**.
- The writable controller for a pane determines that pane’s PTY cell geometry; observers **fit/letterbox** rather than silently resizing everyone.
- Disconnect → defined lease transfer (or clear lease) with **visible read-only / takeover** chrome.

#### 2.8.5 Persistence semantics

| Event | Meaning |
|-------|---------|
| **Detach** | Live PTYs keep running in the long-lived server; client disconnects |
| **Server crash / reboot** | **Not** detach; processes may die |
| **Cold resurrection** | Later/separate; restore layout/cwd/metadata only; **never** auto-rerun recorded commands without confirmation |

#### 2.8.6 Layout contract

- Ratios in model; deterministic integer-cell rounding; stable child order.
- Explicit **min pane geometry**; reject/fail **atomically** if split/resize cannot satisfy minima.
- Close collapses parent deterministically; last pane closes window; last window ends session only under a **named** policy.
- Swap/move preserves pane identity.

#### 2.8.7 Control plane (Phase 2, not “later”)

- Versioned **same-user Unix socket** (0600 / runtime dir).
- Request IDs; snapshot + ordered event stream; resnapshot after gap/reconnect.
- Structured argv/cwd/env spawning (**no** shell command strings).
- Backpressure and stale-ID/version errors are first-class.
- CLI is a thin client. Enables Termwright/control E2E and Herdr interop without screen scraping.

#### 2.8.8 Herdr boundary

Expose **generic mux primitives** only: list/snapshot, split, focus, resize, swap, zoom, read visible/recent, send input, titles/metadata, lifecycle/output/activity events.

**Do not** absorb agents, worktrees, prompts, workflow status, or plugins. Herdr remains cockpit/orchestrator; Prism remains emulator + mux. Prefer a documented adapter/mapping over duplicating Workspace semantics.

#### 2.8.9 UX

- **`prismattyc-host` is the sole Phase 2 product compositor.** Nested `prism` remains single-pane fidelity / Termwright harness unless a later decision adds limited nested mux.
- No mandatory prefix tax: discoverable chords + command palette default; optional leader compatibility profile.
- Compact tabs/pane titles; focus ring not color-only; unseen-output/exit badges; keyboard reachability; reduced motion / high contrast.
- **No continuous idle animation** (aligns empty-rich / classic idle posture).

#### 2.8.10 Input and copy

- Focused pane receives keyboard / app mouse (ADR-0003: Shift remains host mouse escape).
- Selection and scrollback stay **client-local per pane** (ADR-0001 policy per client view).
- Crossing pane borders does **not** create a single textual selection in v1.
- Multi-pane clipboard concat only via **explicit** later command, not implicit drag.
- Focus reporting to children only for the client holding **input authority**.

#### 2.8.11 Failure model (must specify in implementation ADRs)

Child exit; last-pane close; server loss/reconnect; stale client/ID; event gap; incompatible protocol; PTY backpressure; too-small layout; resize/write failures; slow observer. **Never** let one pane starve the UI or other panes.

#### 2.8.12 Proof (required acceptance gates)

**Model / unit (default-on)**

- Geometry conservation, non-overlap, minima; stable pane identity through split/swap/close.
- Mutation probes: wrong-pane input, resize, or focus must not leak into another pane’s PTY or screen.
- Per-pane classic fidelity remains green for the claimed matrix version.

**Control plane / multi-PTY (deterministic harness)**

- Live multi-PTY input routing + **SIGWINCH / winsize** per focused and non-focused panes.
- Snapshot + ordered events; **resync after gap/reconnect** (resnapshot).
- **Stale-ID** and **incompatible protocol-version** rejection with structured errors.
- **Slow-observer / backpressure** fairness: one stuck client must not starve UI or other panes’ I/O.

**Windowed host proof (`prismattyc-host` is not Termwright-driven)**

- Deterministic **offscreen compositor / event harness** (synthetic input + paint/state asserts) for multi-pane host logic.
- **OS screenshot dogfood** for chrome/beauty (human or scripted capture).
- Nested **Termwright** remains the harness for **shared VT / nested single-pane** regression. It is **not** windowed mux product proof.

**Operator evidence**

- **A-6** task set in §5.6.1 before large Phase 2 implementation spend.

---

## 3. User stories

Acceptance-oriented stories (from operator-b brainstorm, lightly edited).

### 3.1 Classic and fidelity

**US-1 — Supported classic workloads**
**Given** a classic CLI/TUI workload and control families in the versioned **supported fidelity matrix**, **when** a user runs a listed shell, editor, monitor, or nested multiplexer case, **then** the matrix-listed screen state, keyboard input, and exit behavior match the named reference terminal **with no Prism-rich dependency**. Resize and alternate-screen behavior are included only in releases whose matrix explicitly lists those families; behavior outside the matrix carries no parity claim.

**US-2 — Scrollback and copy**
**As** a CLI user, **I want** bounded, ordered scrollback plus stable selection/copy, **so that** output remains recoverable without corrupting the live grid.

**US-3 — Classic-only when rich is absent**
**Given** no Prism capability reply, **when** an application probes or emits only classic fallback, **then** the session remains usable and **no control payload appears as text**.

### 3.2 Opt-in rich

**US-4 — Negotiated rich features**
**Given** a compatible capability reply, **when** a rich app uses a negotiated feature/version, **then** Prism renders within explicit attachment, z-order, focus, and selection rules; unsupported features degrade to classic output.

**US-5 — Idle classic path**
**As** a classic-only user, **I want** rich features to impose no active animation clock or meaningful idle-path work, **so that** the optional surface does not tax ordinary shells.

**US-6 — App author DX**
**As** an application author, **I want** versioned protocol docs, typed examples, malformed-input rules, and a reference fallback client, **so that** integration is testable on Prism and non-Prism terminals.

### 3.3 Multiplexing

**US-7 — Panes / sessions / detach**
**As** a multiplexer user, **I want** panes/windows/sessions **and detach/reattach** as part of the Phase 2 product claim, **so that** process lifetime and screen state are independent of one view **without changing PTY ownership semantics**. Implementation may sequence **2A** (in-process composition) then **2B** (detach durability); 2A alone does not complete US-7.

### 3.4 Maintainability

**US-8 — Regression gates**
**As** a maintainer, **I want** reproducible locked CI, real-PTY tests, parser fixtures/fuzzing, and a **published fidelity matrix**, **so that** classic regressions **block** rich-feature progress.

---

## 4. Implementation decisions

### 4.1 Product hard gates

| ID | Decision |
|----|----------|
| I-1 | **Classic fidelity gates are bounded by I-20:** a Phase 0A subset exit and a matrix-scoped product release gate. Rich work cannot redefine or bypass classic behavior. |
| I-2 | Native host binary only for normal use (no required browser engine). |
| I-3 | Richness only after capability negotiation; default path is classic VT/xterm-class behavior. |
| I-4 | Single **PTY writer** ownership; bounded parser/state allocations. |
| I-5 | Prism is **not** an agent orchestration runtime. |

### 4.2 From Phase 0 stack (label state carefully)

| ID | Decision | State |
|----|----------|--------|
| I-6 | Rust 2021 workspace, MSRV 1.90, six crates; **emulator isolated from rich types** | **On main** (`4540d22`) |
| I-7 | CI: least-privilege GHA, concurrency cancel/timeout, locked fmt/check/test/strict clippy | **On main** (`5e8a951`); live GHA green run `30417135305` @ `63dbac8` |
| I-8 | Capability first cut: bounded APC `Prismattyc;` namespace, ASCII ≤4096, ids, version+feature registry, classic fallback, tmux pass-through limitation documented; **no encoder/decoder/rich emission required yet** | **On main** (`b5d17d3`); reviewable first cut (not forever-frozen production protocol) |
| I-9 | Classic vertical slice: real native PTY → **vte** subset → bounded styled primary grid + **10k-line** scrollback → terminal-backed ANSI renderer → raw input; host raw/alt-screen **RAII restore**; zero-size terminal → 80×24 | **On main** (via stack tip `e56d868`) |
| I-10 | Unsupported controls: **safe containment** (must not leak payload as text, crash/stall, or corrupt the grid). Ignoring is **not** permanent acceptance—gaps remain named in the fidelity matrix | Planned matrix |
| I-11 | Software/terminal-backed render first; GPU/Wayland/X11 later features | Planned for production backends |
| I-12 | **Historical (Phase 0/1 sequencing):** single-process MVP first. **Superseded for Phase 2 product intent by §2.8:** 2A may remain single-process composition; **2B requires a long-lived server + attachable client** for detach durability. Remote multi-machine transport remains Later. | On main (41: `prismattyc-mux-server` + attach/detach) |
| I-13 | License **TBD** (owner)—not an implementer default | Owner |

### 4.3 Hybrid (product constraints; deep freeze in)

| ID | Decision |
|----|----------|
| I-14 | Anchors: cell-rect + viewport overlay first |
| I-15 | One primary caret per pane |
| I-16 | Grid-native default selection |
| I-17 | Empty rich layer: no animation clock / meaningful idle work |

### 4.4 Product-risk decisions

| ID | Decision |
|----|----------|
| I-18 | Run the Phase 0B differentiator spike and discovery gate **before** committing to full rich-platform investment; the spike stays experimental, requires the named Spike Baseline v0, and cannot weaken classic release gates |
| I-19 | APC is a first-cut hypothesis until the transport conformance matrix covers direct use, SSH, tmux pass-through states, and nested paths; retain, wrap, or revise the envelope based on evidence |
| I-20 | Phase 0A exit means Spike Baseline v0 fixtures green with no open P0 in that subset; the **classic product release gate** is separately a versioned, published supported matrix green for the claimed version, with named reference terminals, workloads, protocols, and exclusions—not an undefined claim of universal xterm parity |
| I-21 | Product and competitive claims require linked evidence; internal enthusiasm and implementation completion are not adoption validation |
| I-22 | Rich validation and classic/mux validation are separate decisions: a failed rich gate does not automatically kill or approve Phase 2; mux proceeds only with independent A-6 evidence and the required classic baseline |

---

## 5. Testing decisions

### 5.1 Principles

| ID | Decision |
|----|----------|
| T-1 | Classic path tests are default-on; **classic regressions block rich progress**. |
| T-2 | CI is a merge gate (locked workspace). |
| T-3 | Runtime acceptance includes **real PTY smoke** through child → parse → grid → render—not pipe-only simulation alone. |
| T-4 | Exact-head review: re-run full gate on the claimed SHA. |
| T-5 | **No invented numeric latency/overhead SLOs** until baselines exist; define **measurement categories and gates first**, freeze thresholds from data. |
| T-6 | Rich tests are additive and gated behind classic fallback / empty-layer tests; **no rich failure may blank or freeze the classic grid**. |

### 5.2 Required per change (Phase 0+)

- `cargo fmt --all -- --check`
- `cargo check --workspace --locked`
- `cargo test --workspace --locked`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- Explicit `prism` binary build
- Docs/rustdoc where public API changes

### 5.3 Current main evidence (Phase 0A stack)

At main tip **`e56d868`** (rebased onto PRD freeze), **16** tests covering: grid wrap/scroll/erase/cursor; VT cursor/erase/SGR; unsupported OSC **containment**; deterministic/ANSI render; **capability semantic type tests** (not full APC decoder round-trips); zero-size fallback; real `/bin/sh` PTY integration. Exact-head gate re-verified by operator-a at merge. Fixture mapping: [spike-baseline-v0.md](spike-baseline-v0.md).

### 5.4 Build out (planned testing program)

- Recorded/golden VT fixtures
- Fragmented-sequence and malformed/oversized control tests
- **Protocol decoder layer (future):** APC round-trip, malformed/oversized/fragmented capability bodies, reject rules—**not claimed present** until a decoder lands
- Parser fuzzing
- PTY lifecycle failure tests
- Differential checks against **named** reference terminals
- **Published fidelity matrix** covering at least: Unicode width/graphemes, resize/SIGWINCH, alternate screen, mouse modes, bracketed paste, keyboard protocols, color, nested tmux, scroll regions, process exit/error restoration
- **Selection/copy:** basic grid selection/copy acceptance in **Phase 1** (not required for Phase 0 vertical slice); enhanced search/structural selection later

### 5.5 Success-metric *categories* (thresholds later)

| Category | Intent |
|----------|--------|
| Fidelity-matrix pass rate | Zero P0 classic regressions on supported matrix |
| PTY/parser stability | No crash/hang; bounded memory under flood |
| Input→render latency distribution | Capture baselines first; freeze numeric thresholds only after data |
| Idle classic-path CPU/alloc | Baselines with rich unused; freeze thresholds after data |
| Capability fallback conformance | **App-side guarantee:** no affirmative reply ⇒ classic-only. Representative host/tmux pass-through belongs in a **conformance matrix**—not a claim that all foreign hosts ignore APC |
| Exit/error restore | Deterministic host terminal restore (RAII) |
| Example-client conformance | Documented client behaves on Prism and non-Prism |

### 5.6 Product-validation and transport gates

Each result is recorded as **pass**, **fail**, or **inconclusive**, with evidence
links. “Interesting” or “nice to have” feedback does not count as material pain.
A material case must show a recurring task plus at least one observable cost:
duplicated implementation, a separate companion UI, abandoned capability,
recurring maintenance/support work, or task failure.

| Gate | Pass | Fail | Inconclusive / next action |
|------|------|------|----------------------------|
| Discovery | Initial minimum of five eligible interviews; at least three authors in one recruitable segment independently demonstrate a material case | Across the initial and one additional five-author cohort, fewer than three material cases cluster in any segment | Any first cohort below the pass threshold (including zero, one, or two cases), conflicting segments, or shallow evidence: refine the segment/rubric and run one additional cohort; no rich proceed decision |
| Independent integration | At least two eligible non-core authors complete the documented spike integration without Prism-internal changes; both classic fallbacks pass safety checks | No eligible author completes after one docs/support revision, or any unresolved fallback safety defect remains | One completion or both require undocumented implementer intervention: revise docs/API once and repeat; no proceed decision |
| Differentiator value | At least two eligible authors complete the same target task in rich and fallback modes, and each ties the rich result to a material case without a safety defect | Neither author identifies material task value, or the rich path prevents completion / damages fallback | Mixed value, segment mismatch, or usability failure: narrow the use case and repeat once; no proceed decision |
| Alternatives | A versioned `docs/alternatives.md` cites evidence, identifies substitutes/complements/transports, and states why the candidate wedge remains unmet | A current approach meets the wedge without Prism-specific host work and no defensible interoperability gap remains | Evidence or product fit is ambiguous: narrow the claim and collect missing evidence |
| Spike baseline | Every enumerated Spike Baseline v0 fixture passes at a frozen head; missing features match the allowlist | Any safety invariant fails | Missing infrastructure or nondeterministic result: fix the harness; no user-facing spike |
| Supported fidelity | Versioned matrix names reference terminal(s) and versions, workloads, control families, expected behavior, exclusions, and passing evidence | Any P0 behavior in the declared matrix fails | Matrix or reference is incomplete: Phase 1 release remains blocked |
| Capability transport | Every enumerated direct/SSH/tmux/nested case safely falls back; positive negotiation also passes where that case is declared capable | Any payload leak, hang, crash, grid corruption, or false-positive capability claim | Environment or pass-through state cannot be reproduced: mark unsupported pending evidence; do not claim capability |
| Classic/mux value (A-6) | Initial minimum of five eligible session operators; at least three in one segment show recurring material pain, and two complete the **§5.6.1** bounded workflow prototype with distinct value over their external mux | Across the initial and one additional five-operator cohort, fewer than three material cases cluster or fewer than two users complete the §5.6.1 workflow with distinct value over alternatives | Any first cohort below either pass threshold (including zero cases/completions), a fragmented segment, or prototype usability failure: narrow/revise and run one additional cohort before large Phase 2 investment |
| Product checkpoint | Owner records **proceed with production rich work** only when discovery, integration, differentiator, alternatives, spike, supported-fidelity, and transport gates pass; Phase 2 separately requires A-6 and supported fidelity | Any required gate fails: stop or reposition the affected investment | Any required gate is inconclusive: no proceed decision; run only the named next action |

#### 5.6.1 A-6 bounded workflow prototype (frozen task set)

Eligible session operators complete this **repeatable** task set on a Prism mux
prototype (or scripted wizard when 2A/2B are incomplete). Record: misroutes,
wall-clock, recovery steps, and whether they would drop their external mux for
this workflow.

1. Create **three panes** in one window (discoverable UI or documented chords).
2. Discover **focus** and **resize** without documentation hand-holding beyond a one-page card.
3. Run a **distinct marker command** in each pane (unique printable string).
4. **Copy** history/output from at least one pane (host selection / clipboard).
5. **Close** one pane and confirm reflow does not corrupt remaining PTYs.
6. **Close the GUI / detach** (2B path or simulated server-keep) such that processes should remain alive when detach is in scope.
7. **Reattach** (when 2B is available) and confirm PTYs still alive with markers intact; if only 2A is under test, record that detach steps are blocked and score 2A-only partials separately.
8. **Switch** a named session (or create/switch second session) and return.
9. Locate **unseen output** (badge or scroll) on a background pane.

**Compare** against the operator’s current external multiplexer (tmux/Zellij/WezTerm/Herdr/etc.) on the same task list. A “completion with distinct value” must finish the applicable steps and state at least one material advantage or equal capability with lower friction—not mere enthusiasm.

For every transport declared supported, **100% of the versioned, enumerated
conformance cases** must preserve fallback safety: no control payload leakage,
hang, crash, or classic-grid corruption. This is not a claim over all possible
byte streams or middleboxes; fuzz/property testing remains additive. Positive
APC negotiation is required only on paths the matrix explicitly declares
capable; unsupported paths must time out into classic-only behavior.

The transport artifact must use this schema and freeze expectations **before**
execution. Record exact Prism, SSH, and multiplexer versions with the result.

| Row | Path | Prism role | Pass-through configuration | Expected result | Required safety assertions |
|-----|------|------------|----------------------------|-----------------|----------------------------|
| T1 | Local app → Prism | Direct terminal host | N/A | Negotiate | Correlated bounded reply; no leak/hang/crash/grid damage |
| T2 | Local app → tmux → Prism | Terminal host outside tmux | Default/off | Timeout → classic-only | No payload text or false-positive capability; bounded timeout |
| T3 | Local app → tmux → Prism | Terminal host outside tmux | Documented Prism wrapper + explicit tmux pass-through enabled | Negotiate | Correlated bounded reply; no leak/hang/crash/grid damage |
| T4 | Remote app → SSH channel → Prism | Local terminal host | No intermediate mux | Negotiate | Correlated bounded reply; no leak/hang/crash/grid damage |
| T5 — named nested row | Remote app → tmux (default/off) → SSH channel → Prism | Local terminal host outside remote tmux | Remote tmux default/off | Timeout → classic-only | No payload text or false-positive capability; bounded timeout |

Additional rows may cover local-over-remote tmux, double tmux, screen, or other
middleboxes, but “nested” cannot be claimed without at least T5 or another
equally concrete named path. If the implementation required by a row does not
exist, mark the row **not run / gate inconclusive**; do not change its expected
result after observing the test.

---

## 6. Out-of-scope items

### 6.1 Permanent product non-goals

- General-purpose **IDE / code editor**
- Embedded **browser / Electron** or HTML/CSS/JS app model
- **Forced** rich adoption for all apps
- Closed **AI-first terminal product** positioning
- **Agent orchestration runtime** (messaging, identity fabric, ticket systems)
- Inventing a default **license** without the owner

### 6.2 Phase 0 / near-term non-goals (historical — scoped to original validation PRD)

These applied to **Phase 0 / Phase 1 product-validation scope**. They must **not** be read as forbidding an **owner-authorized Phase 2**:

- ~~Daemon/client split, remote attach/sharing~~ → **Phase 2B** local detach/server is in scope for the Phase 2 product claim; **remote** transport remains Later
- Production GPU / Wayland / X11 backends (still Later / D9; windowed software host is Phase 1.5)
- ~~Full mux productization~~ → **Phase 2** when authorized (§2.8); rich UX polish remains Phase 3+
- Production rich decoder/emitter (until Phase 3)
- Guaranteed APC pass-through through arbitrary multiplexers

### 6.3 Deferred fidelity — **not** out of product scope

These are **required before a “modern-terminal compatible” milestone**, even if missing from the first vertical slice:

- Alternate screen completeness
- Mouse protocols
- Live SIGWINCH / robust resize
- Unicode width / grapheme correctness
- Broader keyboard/paste/color/scroll-region coverage

Track via the fidelity matrix; do not list them as charter non-goals.

---

## 7. Implementation state snapshot

Canonical table: **§2.7**. Always `git rev-parse origin/main` before status claims.

---

## 8. Open questions

1. Does the candidate app-author wedge recur in discovery, or is Prism solving an implementation-led problem?
2. Which existing approaches are genuine substitutes, complements, or transport layers for Prism's proposed rich surface?
3. Does APC survive the supported transport matrix well enough to keep, or does the envelope need wrapping/revision?
4. Which named reference terminal(s) and exact VT subset define Phase 1 “modern-terminal compatible”?
5. Scrollback default/config surface beyond the early 10k-line host.
6. ~~Ordering of mux vs rich~~ → **Answered for product intent:** Phase 2 mux (2A→2B) after Phase 1.5 windowed host; Phase 3 rich remains capability-gated and separate (I-22 / A-6).
7. License selection (owner).
8. When to freeze latency/overhead numeric thresholds after baselines.
9. ~~Controller lease grain~~ → **Frozen §2.8.4: per pane.**
10. Phase 2 open: last-window/session end policy; exact discoverable key chords / palette IA; whether nested `prism` ever gains limited mux.
11. Phase 2 open: multi-client takeover UX details; observer letterbox algorithm; control-plane protocol versioning scheme.

---

## 9. Document control

| Version | Date | Notes |
|---------|------|-------|
| 0.1 | 2026-07-28 | Initial draft (spine + stack harvest) before full brainstorm body |
| 0.2 | 2026-07-28 | Merged operator-b `PRD-BRAINSTORM` (`01KYNH3DD783AGZFHBM6W1TFB4`) |
| 0.3 | 2026-07-28 | Addresses `PRD-REVIEW: NEEDS_CHANGES` (msg `01KYNH8AF1C1CTBK38MFVJWFV1` on main `53136e0`): evidence table, decoder-test phasing, safe containment wording, app-side capability guarantee, measurable metrics, P4 maintainer persona, selection/copy phasing |
| 0.4 | 2026-07-28 | Residuals from reviews at `9132b63` / `11eeb3a`: neutral §1.2 gap wording; meta version text; strip Markdown trailing-space hard breaks (`git show --check` clean) |
| 0.5 candidate | 2026-07-28 | Adds product hypotheses and validation gates; revised for adversarial R1–R9 |
| 0.5 | 2026-07-28 | Adversarial PASS by operator-a on product-gate content at `f993eaa`, identical on main at `d117fd1` (R1–R11 and N1–N3 closed); exact-head re-review required after material edits |
| 0.6-draft | 2026-08-11 | Phase 2 mux product freeze §2.8; NEEDS_CHANGES N1–N6 at `4e4898d` addressed (per-pane lease, proof gates, A-6 task set, research links, D2/roadmap consistency, no trailing-space hard breaks); **re-arm operator-b PRD-REVIEW** |
| 0.6 + snapshot | 2026-08-12 | §2.8 frozen (operator-b PASS @ `3896a3b`, recorded in §2.8). §2.7 snapshot + I-12 state refreshed for the wrap @ `3a923a9`: pack published, A-6 PASS deferred, §5.6 criteria untouched. Dual review with operator-a. |

**Supersedure:** Edit `docs/PRD.md` in git; supersede VectorVault `task_id=prism-prd` on material edits.
