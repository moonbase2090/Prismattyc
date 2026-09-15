# Hybrid rendering rules — v0 freeze

**Status:** Phase 0 design freeze. Binding for implementation until a
superseding ADR or PRD amendment. The Phase 3 wire/paint encoding this
freeze deferred is now scoped by [ADR-0013](adr/0013-rich-surface-v1.md)
(entry). Reserved-row workspaces and rich v2 ownership are superseded by
[ADR-0014](adr/0014-rich-surface-v2-fabric.md); the v0 rules remain binding for
cell rectangles, viewport overlays, and protocol `0.1`/`0.2`.
**Kind:** Product composition rules for classic grid + opt-in rich layer.
**Not:** a rich paint protocol, wire encoder, or GPU backend.

Related: [capability negotiation](capability-protocol.md) and
[architecture](architecture.md).

## Invariants

1. The classic cell grid is **always present** and is authoritative for
   unaware applications.
2. Rich content is **opt-in**, **attached**, and **query-gated** (no rich emit
   without a successful capability reply — see capability protocol).
3. Empty rich layer cost ≈ **one null check** on the classic hot path: no
   animation clock, no hit-test tree walk, no compositor allocation.
4. Only **one primary input caret** per pane at a time.
5. Default selection is **grid-native**.
6. Rich failure cannot blank, freeze, or replace the classic grid.
7. Host chrome (pane borders, status, mux UI) is never app-owned and cannot be
   painted over by child content from another pane.

## Layers and z-order (primary screen)

Bottom → top within a **single pane** (unambiguous freeze):

| Z | Layer | Owner | Notes |
|---|-------|--------|-------|
| 0 | Classic cell grid | Host | Includes scrollback-visible primary rows |
| 1 | Cell-rect rich attachments | App (via protocol) | Clipped to **pane content rect** and cell bounds; scroll with grid |
| 2 | Viewport overlay rich (HUD) | App | Fixed to visible viewport; clipped to pane content rect; not in scrollback |
| 3 | Selection highlight | Host | Above app content; **below** primary caret |
| 4 | Primary caret | Host or rich focus target | Exactly one; sole input caret (see Cursor) |
| 5 | Host chrome | Host | Borders, status, mux UI — **topmost** in the pane stack |

**Rules:**

- App content (z0–z2) is always clipped to the pane content rect; apps cannot
  paint into other panes or over host chrome (z5).
- When the rich layer is empty, layers 1–2 are absent (null), not transparent
  full-screen surfaces.
- Cell-level classic extensions (future Sixel/Kitty **as grid paint**) remain on
  z0 until a feature is explicitly reclassified.
- Selection never sits above the primary caret; chrome never sits under app paint.

## Alternate screen

When the child enters the **classic alternate screen** (standard DECSET 1049-class
behavior, once implemented in the fidelity matrix):

| Rule | Decision |
|------|----------|
| Scrollback | Alternate screen has **no scrollback** (classic semantics) |
| Primary rich state | **Suspend and preserve** primary-screen attachments (cell-rect and viewport). While alt is active they are excluded from paint, hit-test, and update delivery; they are **not** detached and are **not** scrollback snapshots |
| Alt-scoped attachments | Separate ID space and state from primary. Apps may attach only with explicit alt-screen scope (not in v0 spike); primary IDs must not paint on alt |
| Viewport overlay (alt) | **Allowed** only if capability advertised and app re-requests for alt scope; default off |
| Cursor / selection | Same one-caret and grid-native defaults on the alternate grid |
| Exit alt-screen | **Destroy** all alt-scoped attachments; **resume** preserved primary attachment state (same IDs/geometry policy as before enter); repaint primary |

**v0 implementation note:** Spike Baseline v0 allowlists missing child alt-screen
modeling. These rules apply when alt-screen lands; they do not expand Phase 0A
fixtures.

## Attachment model (frozen first kinds)

| Kind | Feature id | Geometry | Scroll | Hit-test default |
|------|------------|----------|--------|------------------|
| Cell rect | `hybrid.attach.cell_rect` | Inclusive cell rectangle in pane coords | Moves with grid rows | Does **not** claim pointer unless `pointer=claim` |
| Viewport overlay | `hybrid.overlay.viewport` | Viewport-local rectangle (cell or pixel later) | Fixed to visible viewport | Claims pointer only if `pointer=claim` |

**Deferred (not frozen):** line-relative sticky anchors, full-pane rich-primary
takeover, multi-pane shared surfaces. ADR-0014 separately freezes a top
reserved-row workspace that shrinks the guest grid; it is not either attachment
kind in this table.

### Attach / update / detach lifecycle (semantic)

```text
App                         Host
 |  capability reply OK      |
 |<--------------------------|
 |  attach(kind, id, geom…)  |
 |-------------------------->|
 |  update(id, content…)     |  (may repeat)
 |-------------------------->|
 |  detach(id) or session end|
 |-------------------------->|
 |  region dropped; grid OK  |
```

- Attach without capability reply → host **ignores** (classic-only).
- Invalid geometry → drop region, keep grid; no PTY diagnostic spam.
- Detach or child exit → all of that session's rich regions dropped.
- Wire encoding of attach/update/detach is **out of scope for** (Phase 3 /
  0B experimental); this freeze is the composition contract.

## Cursor ownership

| Mode | Primary caret | Classic host cursor | When |
|------|---------------|---------------------|------|
| Classic (default) | Host grid caret | Visible at grid position | No rich focus |
| Hybrid passive | Host grid caret | Visible | Rich regions present but none hold keyboard focus |
| Rich focus | Rich target caret **only** | **Hidden** (not drawn as a caret) | App took focus via negotiated `input.rich_focus` |

**Rules:**

- Focus handoff is **explicit** (protocol); never implied by hover alone.
- Escape / release-focus returns caret to the host grid and keyboard to the child PTY.
- Ambiguous dual carets are a **P0** composition bug: in Rich Focus the classic
  caret must not be visible. A host debugging marker, if any, must be **opt-in,
  non-default, and not styled or hit-tested as a caret**.
- Unaware apps never leave Classic mode.

## Selection

| Case | Behavior |
|------|----------|
| Default | Cell-grid selection (char/word/line); works under empty or non-claiming rich overlays |
| Rich claims selection | Only if region sets `selectable=rich` **and** is hit-tested; then selection may follow rich text runs |
| Copy buffer (v0) | Prefer **plain-text** serialization useful in shells, editors, tmux |
| Multi-format clipboard | Deferred (later) |
| Selection vs caret | Selection is z3 (above app content, below caret); does not transfer keyboard focus |

**Phase note:** basic grid selection/copy product acceptance is **Phase 1**
(PRD US-2). This freeze defines the hybrid policy so Phase 1 and 0B do not invent
conflicting rules.

## Mouse and keyboard focus

1. Mux decides **which pane** is focused.
2. Inside the pane: hit-test top-most rich region with `pointer=claim`; else
   classic (mouse reporting / selection / paste).
3. Keyboard goes to the **focused target**: child PTY by default; a rich widget
   only after explicit rich focus.
4. Classic mouse protocols continue when the rich layer does not claim the event.

**Invariant:** an app that never enables rich features gets the same mouse
behavior as a high-quality xterm-class emulator (within the published fidelity
matrix for that release).

## Scrollback and frozen rich snapshots

| Content | In scrollback history? | v0 decision |
|---------|------------------------|-------------|
| Classic cells | Yes (bounded) | Already implemented for primary screen |
| Cell-rect rich (live) | No live tree in history | While **any** cells of the attachment rect remain in the visible primary grid, **translate + clip** the whole attachment with the grid (no row-granular detach state). Detach the attachment only when its rect is **fully** outside the visible primary grid (or on explicit detach / session end) |
| Frozen snapshot | Optional later | **Deferred:** may store a plain-text or static image snapshot for scrollback; not required for 0B. Suspended primary state on alt-screen is **not** this mechanism |
| Viewport overlay | Never | HUD is viewport-local only |

**Rules:**

- Clip, partial visibility, and detach **never** delete or suppress underlying
  classic cells. Classic cells remain the paint and recovery surface under the
  attachment.
- Any 0B task-level rich attachment **must** already have a classic-cell fallback
  so partial clip or full detach cannot blank the user's content.

**Rationale:** live rich trees in scrollback create unbounded memory and hit-test
complexity. Classic text remains the durable recovery surface.

## Damage and performance

| Path | Budget intent (no numeric SLO yet — PRD T-5) |
|------|-----------------------------------------------|
| Classic-only pane | No rich allocations; damage = cell dirty rects; idle path is PTY + parse + grid |
| Classic with empty rich handle | One null check per frame/path that could paint rich |
| Cell-rect update | Damage union of that rectangle only; do not mark whole grid dirty |
| Overlay animation | Independent rich damage; must not force full-grid classic repaint if cells unchanged |
| PTY reader | Must not stall on rich paint; drop/skip rich frames rather than block the parser |

Full-grid re-render in the current ANSI path is an **MVP implementation
choice**, not a hybrid-model requirement. Hybrid rules assume future damage
tracking can tighten the classic path independently.

## Resize sequence (cell-rect)

```text
Host receives SIGWINCH / pane resize
  → update classic grid dimensions (and PTY winsize when live)
  → for each cell-rect attachment:
       if geom still in-bounds: reflow/clip per attach policy (default: clip)
       if geom fully out of bounds: detach region (no crash)
  → viewport overlays: recompute against new viewport
  → repaint damaged regions only (when damage tracking exists)
```

Child-driven resize protocols remain classic-path concerns; rich geometry is
host-authoritative after the resize.

## Failure / fallback

- Parse/render failure for one region → drop **that** region only; grid stays.
- Host diagnostics go to **chrome or logs**, never injected into the child PTY
  as fake program output.
- Leaving rich mode (detach all / capability timeout) is **reversible** without
  killing the child.

## Accessibility (v0 minimum)

- Screen-reader / accessibility trees for rich content are **not** frozen here.
- **Required classic guarantee:** the grid's plain-text rows remain a usable
  textual representation of the session for tools that only see cells.
- Rich-only information without a classic textual fallback violates PRD US-3 /
  capability fallback rules for any spike that claims a user task.

## Image protocols (Sixel / Kitty graphics)

- **Classic path first:** treat as grid-adjacent classic features when scheduled
  in the fidelity matrix; not hybrid attachments by default.
- Bridging image protocols into `canvas` rich features is a **later** cut and
  needs an explicit feature id.

## Capability bits (already named)

From [capability-protocol.md](capability-protocol.md):

| Feature | Hybrid role |
|---------|-------------|
| `hybrid.attach.cell_rect` | Cell-rect attachments |
| `hybrid.overlay.viewport` | Viewport HUD overlays |
| `input.rich_focus` | Explicit keyboard focus handoff |
| `markup` / `style` / `animation` / `canvas` | Content models inside attachments |

## Non-goals

- Embedding a full HTML/CSS/JS engine.
- Forcing existing TUI toolkits to rewrite for classic workloads.
- Making the classic grid a façade over a browser compositor.
- Freezing wire paint opcodes (Phase 3 / experimental 0B).
