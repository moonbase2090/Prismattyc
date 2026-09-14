# Route Space mutations to focused tests

The router uses two Space selectors before the ordinary full crate suite.
These routes require the Space fixtures introduced by PR #349. Merge that
prerequisite before enabling the routes on main. If a selected group matches
zero tests, the router fails before it runs a mutant.

| Source | First test selection |
| --- | --- |
| `crates/prismattyc-host/src/space_open.rs` | `space_open::tests::` |
| `main.rs`: `open_space_from_host`, `advance_space_opens`, `poll_host_attach_tabs`, `persist_attach_selection` | Exact `space_open_window_tests::delayed_chip_opens_keep_cache_label_and_focus_in_order` |

The state tests cover queue transitions, cache matching, completion, and
persistence fencing. The real-window fixture uses delayed child processes and
checks the cache, current Space, and focus after idle. `App::pump` and
`save_space_from_host` have no route because this fixture does not establish
their mutation coverage. Existing restore and render routes keep their scope.

A first-pass miss or timeout still needs the full suite. Route selection does
not remove identities from the discovered universe. Mutations that cannot be
isolated from an overlapping unlisted function use the full suite. Existing
source-level exclusions remain separate coverage debt.

## Read the timing evidence

The [retained receipt](evidence/mutants-353.json) records binary identity,
source commits, selector test names, and one existing caught-mutant result.

| Observation | Source | Duration |
| --- | --- | ---: |
| Five state tests, cached test executable | #350 implementation `6fa72616` | 0.024 s wall time |
| One exact Space window fixture, cached test executable | #350 implementation `6fa72616` | 10.612 s wall time |
| Failed nested Space fixture for `open_space_from_host` replaced with `()` | #349 `270d5548` | 2.52 s libtest duration |
| Enclosing 722-test suite for that same mutation | #349 `270d5548` | 76.57 s libtest duration |

The cached selector measurements exclude Cargo, compilation, discovery, and
mutation setup. The #350 fixture has additional outcome cases and is not the
#349 baseline. These measurements are not a paired speedup benchmark. Nexus
must measure selector baselines again in the runner used for mutation gating.
The cancelled #349 wave is not a passing mutation gate.

## Verify the route

Run `python3 scripts/mutants-route_test.py`. The small real cargo-mutants
fixture checks both Space selectors, nonzero test discovery, and full-suite
fallback for focused survivors. It uses a synthetic crate to prove routing;
it does not substitute for the host window fixture. The selector identity
assertion must fail when a Space route is removed.

Keep cargo-mutants at the pinned version. Keep one mutant worker and one test
thread. Keep the complete-universe 80% gate and the ordinary full baseline.
Nexus owns light Local Actions and any targeted host mutation sample.
