# Phase 0B — Experimental validation spike

**Status:** Closed — operator-b EXACT-HEAD PASS at `534284b`. Experimental spike only; not a product release.
**Version:** `prism-spike/0.1`
**Host protocol version advertised:** `0.1`
**Prerequisite:** Spike Baseline v0 exit ([spike-baseline-v0.md](spike-baseline-v0.md)).

Related: [capability-protocol.md](capability-protocol.md),
[hybrid-rendering.md](hybrid-rendering.md), [PRD.md](PRD.md) §2.6.

## Purpose

Prove, inside Prism only:

1. bounded capability **query → reply** on the wire;
2. **one** attachment kind (`hybrid.attach.cell_rect`) with explicit
   cursor / selection / input posture;
3. the **same user task** remains usable via classic text when the spike is
   off, times out, or fails.

This does **not** claim modern-terminal compatibility, production protocol
freeze, adoption validation, or independent third-party integration.

## Experimental host flag (off by default)

| Enable | Disable (default) |
|--------|-------------------|
| CLI: `prism --experimental-rich …` | omit the flag |
| Env: `PRISM_EXPERIMENTAL_RICH=1` | unset / `0` / `false` |

When disabled, Prism behaves as the classic Phase 0A path: APC bodies are not
answered, no rich attachments are applied, and classic fixtures remain green.

## Limits

| Limit | Value |
|-------|--------|
| APC body size | ≤ 4096 printable ASCII bytes |
| Capability major | host implements `0.x` only |
| Spike features advertised | `hybrid.attach.cell_rect` only |
| Max concurrent cell-rect attachments | 8 |
| Attachment `text` | printable ASCII without field delimiters (`;` / `=`); length ≤ min(4096, rows×cols, 256); `text` must be the final attach field |
| Geometry | zero-based row/col; rows/cols ≥ 1; clipped to screen |
| Child write queue | bounded (32) outstanding stdin/control writes; control replies use non-blocking enqueue |
| Negotiation | attach accepted only after a capability reply advertising `hybrid.attach.cell_rect` was successfully queued |
| Scroll | cell-rect translates with primary grid scroll; clipped while partially visible; detached when fully outside |

## Timeout / fallback (application side)

| Item | Spike guidance |
|------|----------------|
| Query timeout | Apps should wait ≤ **250 ms** on a local PTY for a reply |
| No reply / malformed / timeout | Stay classic-only for the session (or rate-limited retry) |
| Host experimental off | No reply → classic-only |
| Attachment failure | Drop that region; classic grid unchanged |

Prism itself does not inject diagnostics into the child PTY stream for
malformed control data.

## User task under test

**Task:** show a one-line status label (e.g. `status:ok`) without breaking the
underlying classic session.

| Path | Behavior |
|------|----------|
| Classic fallback | Child prints `status:ok` as normal text (always works) |
| Experimental rich | After capability reply, child may `attach` a cell-rect with the same text; host paints it as a non-caret overlay; keyboard/selection remain classic |

### Cursor / selection / input (this attachment)

Per [hybrid-rendering.md](hybrid-rendering.md) **Hybrid passive**:

- **Cursor:** host classic caret remains primary (attachment is non-caret).
- **Selection:** grid-native default; attachment does not claim selection.
- **Input:** keyboard stays on the child PTY; no `input.rich_focus`.

## Wire (capability)

```text
# query (app → host)
ESC _ Prismattyc;cap;q;id=<nonzero>;max=0.1 ESC \

# reply (host → app), experimental only
ESC _ Prismattyc;cap;r;id=<echo>;v=0.1;features=hybrid.attach.cell_rect ESC \
```

## Wire (spike attachment)

```text
ESC _ Prismattyc;attach;cell_rect;id=<nonzero>;row=R;col=C;rows=H;cols=W;text=<ascii> ESC \
ESC _ Prismattyc;detach;id=<nonzero> ESC \
```

Unknown geometry keys ignored. Duplicate required keys, non-printable bodies,
zero ids, zero geometry, delimiter bytes in `text`, text longer than
`rows*cols`, and oversized bodies invalidate the message (no grid corruption).
Selected protocol version `0.0` receives an empty feature set (cell_rect is
`0.1+`).

## Safety harness

Required automated coverage (see workspace tests):

- Spike Baseline v0 fixtures remain green with experimental **off** and **on**.
- Capability round-trip encode/decode; malformed / oversized / unknown-key cases.
- Fragmented APC collection across feed boundaries.
- Experimental off → no reply bytes generated for queries.
- Attach paints overlay text without leaking control payload into the grid.
- Classic `status:ok` print path still works without experimental flag.

## Non-claims

- Not a supported terminal release.
- Not a transport-matrix pass (tmux/SSH/foreign hosts unproven).
- Not full rich markup/canvas/animation.
- Not independent external-author integration.

## Exit criteria (Phase 0B)

- [x] Flag off by default (`--experimental-rich` / `PRISM_EXPERIMENTAL_RICH`)
- [x] Decoder + encoder with adversarial tests (`prismattyc-protocol`)
- [x] One cell-rect attachment + classic fallback for the same task
- [x] This document published
- [x] E2E harness green on a frozen head (workspace unit/integration tests)
- [x] Live CI green (run `30418912701` @ `534284b`; prior tip `30417940362` @ `1b80e71`)
- [x] No open P0 against Spike Baseline or the spike paths above
- [x] operator-b adversarial PASS closed at exact origin/main `534284b`
