# ADR-0013 — Rich surface scope v1

**Status:** Accepted, superseded for protocol `0.3` and later by
[ADR-0014](0014-rich-surface-v2-fabric.md)
**Date:** 2026-08-13
**Depends on:** [hybrid-rendering.md](../hybrid-rendering.md);
[capability-protocol.md](../capability-protocol.md)

## Context

The rich layer has a frozen composition model (hybrid-rendering.md: z-order,
lifecycle, cursor/selection/scrollback/resize rules, damage budget, failure
isolation) and a capability protocol design (APC `Prismattyc;` envelope,
query-before-emit), proven by a closed spike: one `hybrid.attach.cell_rect`
kind, plain ASCII text, attach/detach only, behind `--experimental-rich`,
in the TTY binary only. hybrid-rendering.md leaves the
wire encoding of attach/update/detach and paint payloads to later work —
that is the design surface this ADR bounds.

This ADR is the first ADR to own the rich surface.

## Decision

Rich **entry** is bounded build + freeze, in this order of work:

1. **Wire protocol v1** (`prismattyc-protocol`): add `update(id, …)`,
   the second frozen attachment kind `hybrid.overlay.viewport`, styled-run
   payloads, and reply limit keys. Advertised protocol version `0.2`;
   `max=0.1` peers get the exact 0B feature set, byte-identical.
2. **Windowed-host adoption** (`prismattyc-host`): per-pane rich sessions
   behind the experimental flag, cell-rect attachments painted at z1 under
   host chrome z5, clipped to the pane content rect.
3. **Harness + transport matrix**: `test-phase3-rich.sh`,
   the capability transport conformance matrix, and the
   `--experimental-rich` flood-freeze fix.

### v1 content model: styled runs only

The v1 payload is **styled text runs** (per-run fg/bg color, bold, italic,
underline, inverse) inside the two frozen attachment kinds. It extends the
0B plain-text grammar without introducing a document model.

`markup`, `animation`, and `canvas` remain **unadvertised** in capability
replies and undefined on the wire. Advertising any of them requires a
superseding ADR. Rationale: styled runs exercise every frozen composition
rule (z-order, damage, scroll translate+clip, alt-screen suspend, failure
isolation) at a fraction of the attack/complexity surface of a markup or
canvas model. The envelope may still change before the wire is
production-frozen.

### Boundaries that hold (unchanged, restated as binding here)

- `prismattyc-protocol` stays dependency-free with no screen/render/mux imports.
  `prismattyc-emulator` never depends on a rich document graph; the APC
  sidecar only delivers bounded bodies upward.
- Classic-off-cost invariant: with the flag off, no `ApcCollector`
  allocation, no rich branches on the paint path, byte-identical output.
- Classic regressions are P0 over rich features; no rich failure
  may blank or freeze the classic grid.
- Query-before-emit: no rich bytes before a granted capability reply.

### Gates: what this ADR does and does not claim

- This ADR does **not** record a product-readiness checkpoint for
  production rich work. Entry work proves the wire, the windowed host
  path, and transport conformance.
- Discovery, independent integration, and differentiator-value gates
  remain open.

## Consequences

- Rich entry work proceeds without overclaiming production readiness.
- The windowed host becomes the rich reference surface; the TTY binary
  remains the parity/claim harness.
- Styled runs give integrators a real (if small) surface to react to.

## Rejected alternatives

- **Open `markup` first**: largest surface, weakest test story, and the
  envelope is not yet production-frozen — wrong first step.
- **Skip the windowed host, extend the TTY binary**: the windowed host is
  the adoption path (ADR-0006) and the only place pane-clipping semantics
  exist; deferring it defers the real composition questions.

## Amendment — `input.rich_focus`

`input.rich_focus` is a **capability + input-plumbing** extension, not a new
advertised content kind. `markup`, `animation`, and `canvas` stay unadvertised.

- Advertised only at protocol `0.2` when the host runs `--experimental-rich`.
  `max=0.1` replies stay byte-identical to the 0B set (key absent).
- The client may request focus for an attached region id. The host grants
  only after an explicit user chord (`Ctrl+Shift+G`); never automatically.
  Esc or the same chord revokes. Pane switch, region detach, and flag-off
  always revoke.
- Forwarded keys are bounded, percent-escaped APC frames (same escape set as
  styled-run text). Classic input is never silently stolen.
