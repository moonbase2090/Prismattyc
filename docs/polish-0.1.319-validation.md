# Validate 0.1.319 polish

This work builds on `1d5810d001a3d2734f7c037cee539eb9d7e62a4a`.
The tests use private daemons, data directories, and displays. The operator's
running daemon and sessions were not restarted. No production release was
published and no installed binary was replaced.

## Functional results

| Area | Evidence | Result |
| --- | --- | --- |
| Release Update | Real CLI with a local HTTPS transport fixture: interrupted download, digest rejection, immutable metadata, install, rollback, retry, next version, and abandoned-stage cleanup | Pass |
| Coordinated Restart | Private native host and MCP client; daemon idle restart and explicit detached restart; active-session and blank-terminal deferrals | Pass |
| Scrollback reflow | Eight focused core tests; replay and snapshot tests; legacy resize-log compatibility; Termwright narrow/wide output | Pass |
| Interrupted Spaces | Native window fixture plus seven explicit no-op controls: pump, dispatch, cache application, focus persistence, save, status publication, helper cancellation/reaping | All seven caught |
| Agent Messages | Queue details, exact secondary-pane navigation, and activation through the native accessibility bus | Pass |
| Accessibility | Native Linux AT-SPI names, roles, and actions; keyboard actions; built-in palette contrast tests | Pass for the tested Linux paths |
| Load check, performed last | 120 seconds, 118 Space switches, 236,000 output lines, repeated resizes, and blank-terminal ownership checks | Pass |

The last full host test run passed 741 tests. The last mux library run passed
409 tests. Later shared-emulator changes passed their focused reflow, snapshot,
and replay tests. Strict workspace Clippy passed. Six earlier Termwright
scenarios are carried forward; the added reflow RPC scenario passed separately.
Termwright 0.2.0 exposes resize through RPC, not YAML steps.

The load check measured a maximum switch-cycle latency of 1,204 ms.
Host RSS peaked at 130.3 MiB. Daemon RSS peaked at 56.8 MiB. This is a bounded
two-minute stress check, not an overnight endurance claim.

The shared Spaces native acceptance run also passed all 43 checks.
Incremental CRAP review passed at the 40 threshold for 135 changed-function
entries. This scope carries forward unchanged code. It excludes the verified
`cfg(test)` window harness and changes that only remove mutation-skip markers.
The renamed source updater carries its prior score after an identical-body
check. This is not a refreshed full-workspace CRAP baseline. The scope and
receipts are retained under `build/polish/crap-*`.

## Review limits

- The release fixture uses local metadata and downloads. The production
  `Moonbase2090/Prismattyc` channel begins at 0.2.0 and was not published early.
- The updater verifies GitHub HTTPS metadata and SHA-256 digests. It does not
  implement independent signature verification or the full TUF protocol.
- Native accessibility actions passed on Linux. Spoken output in Orca or
  VoiceOver and macOS restart behavior still require platform validation.
- A host that displays an exact secondary pane defers restart. The existing
  view cache stores session targets and cannot restore that view exactly.
- An initial native secondary-pane activation attempt timed out. Subsequent
  repeated native and instrumented runs, including the load fixture, passed.
  The fixture retains failure screenshots/status when an attempt fails.
- The seven explicit no-op controls are not the complete mutation denominator.
  The mutation run failed. Host caught 63 of 110 scored mutants (57.3%),
  below the 60% gate. Another 13 host mutants were unviable. The mux baseline
  failed `interactive_scrollback_pages_then_returns_to_tail` before its 161
  mutants ran. Core passed at 76.7%. The operator authorized merging PR #375
  and installing 0.1.319 with a targeted follow-up for these failures.
  Incremental CRAP review passed. The failed mutation result remains recorded.

## Reproduce the new checks

Build the six binaries before running these commands:

```bash
cargo build --workspace --locked
python3 demo/release-update-e2e.py --pmux target/debug/pmux --out build/release-check
python3 demo/restart-idle-e2e.py --bins target/debug --out build/restart-check
dbus-run-session -- python3 demo/polish-e2e.py --bins target/debug --out build/native-check
./scripts/termwright-e2e.sh classic-reflow
python3 scripts/polish-negative-controls.py --out build/negative-check
```

Run the sustained workload after the other checks:

```bash
dbus-run-session -- python3 demo/polish-e2e.py --bins target/debug \
  --out build/soak-check --soak-seconds 120
```

Acquire the repository's heavy-job lock before running native fixtures,
coverage, or mutations. Do not overlap these jobs on the operator's machine.
Local receipts and screenshots for this run are under `build/polish/`.
