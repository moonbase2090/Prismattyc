# Stopped-session cleanup runtime proof

Tested core revision: `0b80ecf05c82136afbddc8fe4f00d74a68407b07`
Runtime: macOS 26.6.2 Tart VM, with a fresh private app profile and daemon
Embedded Activity Monitor: 1.3.1 (`c6996fd88a6543789e7225a70bc8696397d029da1722a4c58f9380f3549e2e11`)

The native host launched the bundled monitor in the VM. Native Quartz key events drove the monitor actions. The PNGs are the host frames passed to presentation (`PRISMATTYC_DUMP_PRESENT`), not desktop-compositor captures. Each frame has a matching saved pane-text capture. `recorder.jsonl` records the action, selection, visible assertions, and daemon session names before and after it. `result.json` records the seven completed steps and empty teardown leftovers.

| Frame | Action | Verified outcome |
| --- | --- | --- |
| [01 stopped rows](01-stopped-seats.png) | Open monitor with four synthetic stopped sessions | All four down rows visible |
| [02 confirmation](02-remove-confirmation.png) | Press `x` on `alpha-seat` | Confirmation asks before cleanup; daemon session still exists |
| [03 cancelled](03-cancelled.png) | Press Escape | Nothing removed; daemon session remains |
| [04 remove one](04-remove-one-cleaned.png) | Confirm with Enter | Row and stopped daemon session removed |
| [05 hide only](05-hide-only.png) | Remove an already-untracked historical row | Row hidden; no daemon session is cleaned up |
| [06 clear-all confirmation](06-clear-all-confirmation.png) | Press `X` with two stopped sessions | Prompt says two tracked sessions; live sessions are refused |
| [07 clear-all result](07-clear-all-complete.png) | Confirm with Enter | Both rows and daemon sessions removed |

Verification:

- `cargo +1.90.0 test -p pmux-activity-monitor-plugin --locked`: 70 passed.
- `cargo +1.90.0 build --release --workspace --bins --locked`: passed.
- `cargo +1.90.0 build --release --manifest-path tools/activity-monitor/Cargo.toml --locked`: passed.
- `python3 scripts/release/plugin_bundle_provenance.py verify --bins <release-binaries> --bundle <candidate-bundle>`: passed; the embedded core revision and package digest match this proof.
- Clean-runtime interaction drive: PASS; all seven assertions passed and `leftovers_after_teardown` was empty.
- The new epoch regression failed at the prior PR head at the live-row visibility assertion, then passed at the corrected revision. The same-epoch dismissal/restart test also passes.
