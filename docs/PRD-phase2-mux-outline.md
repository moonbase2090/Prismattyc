# Phase 2 PRD outline — Multiplexer

**Status:** Working freeze is **[PRD.md §2.8](PRD.md)** (v0.6+).
This file is a short map + ticket budget only.

**Depends on:** Tag `Pre-Mux`, Phase 1.5 MVP (`prismattyc-host`, tip `14b4e8b`+), classic `0.1.1`, [mux-research.md](mux-research.md).
**Gate:** A-6 (§5.6.1 task set) before **large** implementation investment (not before PRD freeze).

## Canonical freezes

See **PRD §2.8** for the full list. Highlights:

1. 2A composition vs 2B detach (detach required for Phase 2 **product claim**)
2. Domain → Session → Window → PaneLayout → Pane
3. Server vs client ownership; zoom as view
4. **One writable controller lease per pane** (not session-level ambiguity)
5. Detach ≠ crash; cold resurrection later
6. Layout geometry contract
7. Control-plane Unix socket in Phase 2
8. Herdr primitives-only boundary
9. Windowed host sole Phase 2 compositor
10. Per-pane input/copy rules
11. Failure model
12. Proof gates (multi-PTY, resync, stale-ID, backpressure, offscreen host harness)

## Ticket budget (estimate, not filed)

~**1 epic + 8–10 tasks** for 2A; **+2–4** for 2B detach → **~9–14** total.
**Authorized 2026-08-11** — epic, tasks filed.

## Review process

operator-b: `PRD-REVIEW` on exact head of PRD v0.6+.
operator-a: address NEEDS_CHANGES → re-arm exact head.
