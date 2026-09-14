# Capability transport conformance matrix

**Status:** Frozen transport-matrix record while rich vision is open.
**Not** a PRD §5.6 Capability-transport pass, **not** a Phase 3 / A-6
gate, and **not** the owner's "proceed with production rich work"
checkpoint.

**Recorded at:** `e6c496d` parent / this PR head. Prism experimental flag
`--experimental-rich`. Protocol 0.2 on TTY + `prismattyc-host`. T2/T3/T6 scripted
in `crates/prismattyc/tests/transport_matrix.rs`.

Schema follows PRD.md:549–564. Rows whose environment cannot be reproduced
here are **inconclusive**; expected results are not rewritten after the fact.

| Row | Path | Prism role | Pass-through | Expected | Result | Safety assertions / evidence |
|-----|------|------------|--------------|----------|--------|------------------------------|
| T1 | Local app → Prism | Direct terminal host | N/A | Negotiate | **pass** | Correlated bounded reply; no leak/hang/crash/grid damage. Evidence: `rich::tests::v1_negotiate_grants_viewport_and_limits`, `rich::tests::v01_query_reply_is_byte_identical_to_spike`, `rich::tests::grant_required_before_attach`, `prismattyc-emulator::experimental_apc_bodies_are_collected_without_leaking_into_the_grid`. Termwright fixture `e2e/rich-attach.yaml` (optional; not required for CI). |
| T2 | Local app → tmux → Prism | Host outside tmux | Default/off | Timeout → classic-only | **pass** | No payload leak, no STAT overlay. Evidence: `transport_matrix::t2_tmux_passthrough_off_is_classic_only_no_leak`. |
| T3 | Local app → tmux → Prism | Host outside tmux | Documented wrapper + tmux `allow-passthrough all` | Negotiate | **pass** locally (tmux 3.5); **inconclusive on CI** (`ubuntu-24.04` tmux 3.4 passthrough semantics — test prints `tmux -V` and skips with that marker) | Wrapped query/attach via `encode_tmux_passthrough`; STAT paints after grant flush locally. Evidence: `transport_matrix::t3_tmux_passthrough_on_negotiates`, `tests::tmux_passthrough_doubles_esc_and_round_trips`. |
| T4 | Remote app → SSH → Prism | Local terminal host | No intermediate mux | Negotiate | **inconclusive** | No SSH fixture in CI. Semantic contract is identical to T1 (latency only). |
| T5 | Remote app → tmux (off) → SSH → Prism | Local host outside remote tmux | Remote tmux default/off | Timeout → classic-only | **inconclusive** | Named nested remote path not reproduced. |
| T6 | Nested Prism-in-Prism | Inner `prism --experimental-rich` under outer `prism` | N/A | Negotiate on inner only; outer classic unless flagged | **pass** | Inner paints STAT; outer classic does not leak `Prismattyc;cap;q`. Evidence: `transport_matrix::t6_nested_prism_inner_negotiates_outer_classic`. |
| T7 | Under a foreign emulator (xterm) | Prism as child of xterm | N/A | Negotiate or classic-only; no leak | **inconclusive** | No xterm in CI. APC is 7-bit `ESC _`…`ESC \`; unaware hosts must not paint the body as text (collector + containment tests cover the Prism side). |

## Required safety (every executed row)

- No control payload leakage into the classic grid.
- No hang, crash, or grid corruption.
- No false-positive capability grant without a flushed reply.
- Flood under `--experimental-rich` must not freeze input/paint (T-6).

## What this file does not claim

- PRD §5.6 Capability transport gate pass.
- Owner authorization to proceed with production rich work.
- A-6 / classic mux value.
- tmux pass-through, SSH, xterm, or double-nested support.

See [capability-protocol.md](capability-protocol.md) (harness checkbox),
`./scripts/test-phase3-rich.sh`, and `./scripts/test-phase3-transport.sh`.
Those scripts are a frozen regression pin. They are not a light-stage
gate. Nightly exercise is `.github/workflows/phase3-rich-nightly.yml`.
