# ADR-0007 — Mux domain model

**Status:** Accepted  
**Date:** 2026-08-11

## Context

The multiplexer needs sessions, windows, panes, layout, a control plane, and detach. Implementation starts with a pure domain model independent of winit/softbuffer so unit tests and an offscreen harness can prove topology without a GUI.

## Decision

### Information architecture

```
Domain (server-side root)
  └── Session (named, opaque SessionId)
        └── Window (tab, opaque WindowId)
              └── PaneLayout (immutable binary split tree)
                    └── Pane (opaque PaneId) → owns PTY+emulator+screen at runtime
```

### Identity

- All control targets use **opaque stable typed IDs** (`SessionId`, `WindowId`,
  `PaneId`) that are **never reused** for the lifetime of a Domain instance.
- Labels and tab indexes are presentation-only and may change.

### Layout

- Binary split tree: `Leaf(PaneId)` or `Split { axis, ratio, first, second }`.
- Ratios are model-space `0.0..1.0` (exclusive bounds at leaves after clamp);
  integer-cell rounding is deterministic and documented.
- Explicit **minimum pane geometry** (cols/rows); split/resize that cannot
  satisfy minima fails **atomically** (no partial mutation).
- Close-pane collapses parent deterministically; last pane closes window;
  last window ends session only under a **named** policy (default: destroy
  session when empty).

### Ownership (runtime binding later)

| Layer | Owns |
|-------|------|
| Domain / server | topology, IDs, authoritative cell geometry per pane |
| GUI client | focus, active window/tab, viewport scroll, selection, hover, zoom-as-view |

Zoom is a **view projection**, never a topology mutation. Focus and
active-tab selection are **not** stored on `Window` / `Session`; use per-client
`ClientView` (or host-local state). Mutations may return a **suggested** focus
target for the acting client only.

**Empty-session policy (default):** destroying the last window of a session
destroys that session.

### Controller lease (API surface in domain; enforcement later)

- At most **one writable controller lease per pane**.
- Domain stores optional `controller: Option<ClientId>` per pane.
- Getters/setters and unit tests land with the domain; multi-client observers
  and takeover UX land with host integration.

### Out of scope for this slice

- Real PTY spawn
- Unix socket control plane
- Detach server
- Host chrome

## Consequences

- `prismattyc-mux` is the sole owner of topology mutations.
- `prismattyc-host` consumes domain snapshots; it does not invent parallel pane graphs.
- Nested `prismattyc` remains the single-pane classic claim harness.

## Acceptance

1. Typed IDs + Domain API: create session/window/pane, list, focus target helpers.
2. IDs never reused after destroy (unit-tested).
3. Single-pane default session on Domain::new() (or explicit `bootstrap()`).
4. `cargo test -p prismattyc-mux` green; no host/winit deps in `prismattyc-mux`.
5. Docs: this ADR + short module rustdoc.
