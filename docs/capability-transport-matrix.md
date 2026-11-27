# Capability transport conformance matrix

**Status:** Frozen transport-matrix record while rich vision is open.
**Not** a PRD §5.6 Capability-transport pass, **not** a Phase 3 / A-6
gate, and **not** the owner's "proceed with production rich work"
checkpoint.

**Recorded at:** `e6c496d` parent / this PR head. Prism experimental flag
`--experimental-rich`. Protocol 0.2 on `prismattyc-host`. T2/T6 scripted
in `crates/prismattyc/tests/transport_matrix.rs`.

The classic TTY host (`prismattyc`) no longer has rich mode. It accepts
`--experimental-rich` for one release and ignores it. T2 and T6 now check
that it keeps capability messages out of the grid; T3 was removed with it.
The windowed host (`prismattyc-host`) no longer has rich mode either, so the
T1 rich evidence below is historical; its rich tests were removed with it.

Schema follows PRD.md:549–564. Rows whose environment cannot be reproduced
here are **inconclusive**; expected results are not rewritten after the fact.

| Row | Path | Prism role | Pass-through | Expected | Result | Safety assertions / evidence |
|-----|------|------------|--------------|----------|--------|------------------------------|
| T1 | Local app → Prism | Direct terminal host | N/A | Negotiate | **pass** | Correlated bounded reply; no leak/hang/crash/grid damage. Evidence: `rich::tests::v1_negotiate_grants_viewport_and_limits`, `rich::tests::v01_query_reply_is_byte_identical_to_spike`, `rich::tests::grant_required_before_attach`, `prismattyc-emulator::experimental_apc_bodies_are_collected_without_leaking_into_the_grid`. |
| T2 | Local app → tmux → Prism | Host outside tmux | Default/off | Timeout → classic-only | **pass** | No payload leak, no STAT overlay. Evidence: `transport_matrix::t2_tmux_passthrough_off_is_classic_only_no_leak`. |
| T3 | Local app → tmux → Prism | Host outside tmux | Documented wrapper + tmux `allow-passthrough all` | Negotiate | **removed** | The classic host no longer negotiates. The wrapper encoding is still covered by `tests::tmux_passthrough_doubles_esc_and_round_trips`. |
| T4 | Remote app → SSH → Prism | Local terminal host | No intermediate mux | Negotiate | **inconclusive** | No SSH fixture in CI. Semantic contract is identical to T1 (latency only). |
| T5 | Remote app → tmux (off) → SSH → Prism | Local host outside remote tmux | Remote tmux default/off | Timeout → classic-only | **inconclusive** | Named nested remote path not reproduced. |
| T6 | Nested Prism-in-Prism | Inner `prism` under outer `prism` | N/A | Classic-only on both | **pass** | Neither host paints STAT or leaks a capability message. Evidence: `transport_matrix::t6_nested_prism_keeps_capability_messages_out_of_both_grids`. |
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
gate. The `phase3-rich` job in `.github/workflows/ci.yml` runs them on every
push, including each commit to main.
