# Capability transport matrix

How capability messages behave on each path between an application and
Prismattyc. Rich mode was removed, so no host negotiates rich features any
more. The rows now check that capability messages never reach the classic grid
and that only a direct native pane answers a query. This is a regression
record, not a production-support claim for tmux, SSH or nested hosts.

T2 and T6 are scripted in `crates/prismattyc/tests/transport_matrix.rs`. Rows
whose environment cannot be reproduced here are **inconclusive**.

| Row | Path | Prism role | Pass-through | Expected | Result | Evidence |
|-----|------|------------|--------------|----------|--------|----------|
| T1 | Local app → Prism | Direct terminal host | N/A | Native pane answers a native query; others stay silent | **pass** | `capability::tests::a_native_pane_hands_queries_to_its_host_and_drops_other_apc_messages`, `prismattyc-emulator::experimental_apc_bodies_are_collected_without_leaking_into_the_grid`. |
| T2 | Local app → tmux → Prism | Host outside tmux | Default/off | Timeout → classic-only | **pass** | No payload leak and no painted attachment. `transport_matrix::t2_tmux_passthrough_off_is_classic_only_no_leak`. |
| T3 | Local app → tmux → Prism | Host outside tmux | Documented wrapper + tmux `allow-passthrough all` | Not supported | **removed** | The classic host does not negotiate. The wrapper encoding is covered by `tests::tmux_passthrough_doubles_esc_and_round_trips`. |
| T4 | Remote app → SSH → Prism | Local terminal host | No intermediate mux | Classic-only | **inconclusive** | No SSH fixture in CI. Native sessions need a direct local PTY. |
| T5 | Remote app → tmux (off) → SSH → Prism | Local host outside remote tmux | Remote tmux default/off | Timeout → classic-only | **inconclusive** | Nested remote path not reproduced. |
| T6 | Nested Prism-in-Prism | Inner `prism` under outer `prism` | N/A | Classic-only on both | **pass** | Neither host paints an attachment or leaks a capability message. `transport_matrix::t6_nested_prism_keeps_capability_messages_out_of_both_grids`. |
| T7 | Under a foreign emulator (xterm) | Prism as child of xterm | N/A | Classic-only; no leak | **inconclusive** | No xterm in CI. APC is 7-bit `ESC _`…`ESC \`; the collector and containment tests cover the Prism side. |

## Required safety on every executed row

- No control payload leaks into the classic grid.
- No hang, crash, or grid corruption.
- No capability grant without a flushed reply.

`./scripts/test-phase3-transport.sh` runs the T2/T6 rows and the wrapper test.
The `phase3-transport` job in `.github/workflows/ci.yml` runs it on every push,
including each commit to main. It is not a light-stage gate.
