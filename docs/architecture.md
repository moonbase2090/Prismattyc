# Architecture (draft sketch)

High-level components and data flow for Prismattyc. This is a **sketch** to make
classic fidelity and opt-in richness coexist without fighting. Nothing here is
frozen until it meets real code.

Related: [hybrid-rendering.md](hybrid-rendering.md),
[capability-protocol.md](capability-protocol.md), charter technical tenets.

## Goals of the architecture

- One native host binary can fully emulate a modern terminal **and** multiplex.
- Classic cell-grid path stays correct and fast even when rich features exist.
- Rich features are discoverable, versioned, and off by default for unaware apps.
- Detach/reattach and multi-pane do not require abandoning the classic model.

## Component map

```text
                        ┌─────────────────────────────┐
                        │  Input (keyboard / mouse)   │
                        └──────────────┬──────────────┘
                                       │
                                       ▼
┌──────────────┐   bytes    ┌──────────────────────────┐
│  Shell / app │◄──────────►│  PTY / process layer      │
│  (in pane)   │            └──────────────┬───────────┘
└──────────────┘                           │ host→app / app→host
                                           ▼
                                ┌──────────────────────┐
                                │  Escape / VT parser  │
                                │  (prismattyc-emulator)    │
                                └──────────┬───────────┘
                     classic sequences     │    Prismattyc-specific /
                     (default path)        │    capability + rich
                                           ▼
                    ┌──────────────────────────────────────┐
                    │           Screen model                 │
                    │  classic cell grid + scrollback        │
                    │  + optional rich attachments           │
                    │           (prismattyc-core)                 │
                    └──────────────────┬───────────────────┘
                                       │
              ┌────────────────────────┼────────────────────────┐
              ▼                        ▼                        ▼
     ┌────────────────┐     ┌────────────────────┐    ┌──────────────────┐
     │  Multiplexer   │     │  Renderer          │    │  Control plane   │
     │  sessions /    │     │  classic grid path │    │  client↔server   │
     │  windows /     │     │  + optional rich   │    │  attach/detach   │
     │  panes         │     │  (prismattyc-render)    │    │  (later)         │
     │  (prismattyc-mux)   │     └────────────────────┘    └──────────────────┘
     └────────────────┘
```

## Layers (conceptual)

### 1. PTY / process layer

- Spawn and supervise one process per pane (or shared process model TBD).
- Resize, signal, and lifecycle ownership live here (and in mux).
- Must not assume the child knows about Prismattyc.

### 2. Escape sequence parser → screen model

- Parse a solid VT/xterm (and common extensions) subset first; expand coverage
  under tests.
- **Default interpretation:** mutate the classic cell grid and scrollback only.
- Sequences that belong to Prismattyc's rich protocol are routed to the protocol
  module; unknown sequences follow established terminal ignore/pass rules.

### 3. Multiplexer

- Sessions, windows, panes, layouts.
- Focus, zoom, split, and navigation.
- Detach/reattach: screen state + scrollback + process attachment must survive
  without corrupting classic behavior.

### 4. Renderer

Two paths, one compositor responsibility:

| Path | Responsibility |
|------|----------------|
| Classic | Draw cell grid + cursor + selection chrome with low latency |
| Rich (opt-in) | Draw retained markup / styling / animation / canvas where attached |

See [hybrid-rendering.md](hybrid-rendering.md) for z-order, cursor ownership,
and input focus rules.

### 5. Client ↔ server control protocol (Phase 2B)

- Same-user 0600 Unix socket for a thin client attached to a long-lived server
  ([ADR-0011](adr/0011-long-lived-mux-server.md)).
- Carries input, resize, and pane control — not a substitute for the app's PTY
  stream.
- Remote attach is a goal, not an MVP requirement.

### 6. Capability / feature discovery

- Apps **query** what Prismattyc supports before emitting rich content.
- Unaware apps never see a behavior change beyond a high-quality terminal.
- See [capability-protocol.md](capability-protocol.md).

## Data ownership (proposed)

| Data | Owner | Notes |
|------|-------|--------|
| Byte stream to/from child | PTY layer | Single writer to child stdin |
| Cell grid + scrollback | Screen model per pane | Classic source of truth for text |
| Rich scene / attachments | Screen model or sibling store keyed by pane | Must not desync from grid lifecycle |
| Focus / layout | Mux | Decides which pane gets input |
| Capability answers | Protocol + host config | Versioned |

## Failure and compatibility posture

- Parser bugs that break classic apps are **P0**.
- Rich-layer bugs must not freeze or blank the classic grid.
- If rich content cannot be rendered, fall back to classic (or a safe textual
  placeholder) rather than failing the whole pane.

## Open questions

1. Single process vs client/server split for MVP (sketch assumes split is later).
2. Which VT coverage matrix we treat as "MVP solid" (xterm, tmux nesting, …).
3. Whether scrollback is per-pane only or can be shared/searchable across a session early.
4. GPU backend timeline vs software-first correctness — **lean recorded in**
   [decisions-v0.md](decisions-v0.md) **D9** (feature-flagged later; optional
   capacity). Concrete API and ship phase still open.
5. How much of Kitty/Sixel/iTerm image protocols we implement vs bridge.

## Next formalization steps

1. Draw this against the real crate boundaries in [workspace.md](workspace.md).
2. Write sequence diagrams for: resize, alternate screen, detach, rich attach.
3. Promote answers to open questions into ADRs when decided.
