# Agent activity and contained plug-ins

The daemon owns the optional headless activity ring. Collection is off by
default. `[mux] experimental_activity = true` or `--experimental-activity`
enables it. Config changes start or stop collection without restarting the
daemon or replacing sessions. See [the feed design](../../../../docs/agent-activity-feed.md).

The monitor UI and connector query logic run as contained plug-ins. The native
`pmux-activity-monitor` executable presents validated frames and forwards its
compatibility query modes. Installation leaves both packages disabled. Endpoint
registration, package grants, and the MCP operator allowlist are separate steps.

## Build and drive the actual packages

Use Launch to prepare a verified production bundle and build matched binaries.
Then run the production driver, preserving its output outside disposable state:

```sh
python3 scripts/test-plugin-runtime.py --bundle "$verify_bundle" \
  --target-dir "$verify_target" --feature production \
  > "$verify_evidence/contained-activity.log" 2>&1
```

Require exit 0 and its final PASS. The driver verifies exact archive digests,
empty initial store, installation disabled, explicit local grants, actual monitor
cells and RGBA tiles, connector queries, direct and supervised activity-only MCP,
default-empty exports, disable/revocation, stale-handle rejection, terminal input,
resize and restore, hot feed epochs, preserved session identities, and verified
offline metadata. It owns its private daemon, sockets, workers, terminal, and
scratch cleanup. It uses the supplied guest bytes without rebuilding them.
The native helper build needs Rust 1.90 and native dependencies. The fixture
mode without `--bundle` additionally needs that toolchain's Wasm standard library.

`scripts/verify-activity.py` predates package provisioning. It does not install
or enable the contained packages, so it cannot verify a fresh installation.
Its old provider-command recipe does not establish current contained viewer
coverage. Actual provider hook proof additionally requires a reviewed installed
provider runtime, its hook profile, and available authentication. Synthetic hook
events establish feed behavior only. Record unavailable provider prerequisites.

## Discover current sessions independently of history

Run the discovery fixture in a clean private proof environment:

```sh
python3 scripts/test-plugin-runtime.py --feature discovery \
  --target-dir "$verify_target" --out "$verify_evidence/discovery" --debug
```

This mode builds candidate guests and matched native binaries. It requires
Rust 1.90 and its `wasm32-unknown-unknown` target. It does not prove official
bundle provenance or provider-hook behavior. Omit `--debug` to exercise optimized
native binaries.

Require exit 0, the discovery PASS line, and `discovery-result.json`. Inspect the
initial, restarted, and live text frames plus the metadata JSON. The fixture
requires current sessions with no hooks or events, an unchanged event cursor,
a 520-event retention gap, fresh monitor views that still list quiet seats,
and periodic discovery and removal in a live terminal. The current-session
snapshot stays inside the existing activity-only grant. It never creates
history records or advances the activity cursor.

`--out` must name a new directory. It preserves its bundle, daemon log, and
proof artifacts after the private runtime is cleaned up. Failed drives retain
the artifacts written before failure. A debug worker can spend longer than
15 seconds compiling its first live view; the fixture allows 45 seconds for
that view to appear.

## Review grants and remote endpoints

Use only a fresh private daemon with the same verified bundle. The production
driver covers local grants. For remote transport and recovery coverage, run the
fixture mode with a separate target and evidence log:

```sh
verify_fixture_target=$(mktemp -d /tmp/prismattyc-fixtures-XXXXXX)
python3 scripts/test-plugin-runtime.py --target-dir "$verify_fixture_target" \
  > "$verify_evidence/contained-fixtures.log" 2>&1
```

This builds real version-matched Wasm fixtures and production guests. Require
its final PASS for socket, command-query and command-tail transports, atomic
legacy endpoint import, no grant or helper execution during import, bounded
catch-up, endpoint removal, helper cleanup, failed readiness, and durable disable.
Do not use fixture guest bytes for production packaging.

To isolate compatibility with a legacy remote page that omits session metadata,
reuse the verified production bundle:

```sh
python3 scripts/test-plugin-runtime.py --feature remote-query \
  --bundle "$verify_bundle" --target-dir "$verify_target" \
  --out "$verify_evidence/legacy-remote-query"
```

Require exit 0, the legacy remote query PASS line, and
`remote-query-result.json` with 300 retained events, cursor `abc1:300`, and
`retained_history_only: true`. This mode proves the connector query path through
the native worker protocol. Use the full fixture mode above for remote views,
tail streams, endpoint removal, and helper cleanup.

For manual review, `pmux plugin endpoint import` previews legacy definitions.
Repeat with `--apply` only after reviewing them. `endpoint add` registers a
transport. `plugin enable ID --version VERSION --allow activity.read:LABEL`
then grants that named endpoint to the exact installed package. Match legacy
`--host` and `--host-cmd` definitions against registered transports before opening
the thin launcher. An imported label alone grants nothing.

`pmux-mcp` exports no plug-ins by default. Its activity-only mode selects the
connector's four declared activity tools and omits identity tools. Explicit
`--plugin` selection remains bounded by enabled packages and their grants.

## Inspect viewer rendering

Pure layout, model, and frame-capture tests live in the guest crate:

```sh
cargo +1.90.0 test -p pmux-activity-monitor-plugin --locked
cargo +1.90.0 test --manifest-path tools/activity-monitor/Cargo.toml --locked
```

The second command tests the thin launcher's arguments. For visual changes,
record the real frontend after installing/enabling the monitor and registering
and granting every requested endpoint. Existing `demo-feed.py`, `record-demo.py`,
and `record-prismattyc.py` do not provision those packages or grants.
Cover each changed tier, including 150x36, 100x30, 64x20, 34x14, and 22x3,
the timeline, and an outage. Inspect recorded frames in a VT emulator.
A cell-only cast does not prove pixel lanes. Pixel proof needs a supported raw
upload response and captured host frames from an allocated desktop.

## Verify bundles and updater migration

Physical metadata must come from each executable before installed forwarding:

```sh
cargo +1.90.0 build --release -p prismattyc-mux --example plugin_bundle --locked
python3 scripts/release/plugin_bundle_provenance.py verify --bins "$verify_bins" \
  --bundle "$verify_bundle" > "$verify_evidence/plugin-bundle.json"
python3 tests/native/source-update-plugin-e2e.py --pmux "$verify_bins/pmux" \
  --verifier "$verify_bins/examples/plugin_bundle" --bundle "$verify_bundle" \
  --out "$verify_evidence/source-update"
```

Require all five consumer identities
and the original guest build receipt to match. The source updater fixture
uses actual CLI and bundle verification with external Git/Cargo shims. Require
all three mux installation requests to share the bundle and build flags, and
corruption to stop before any installation request. It does not install binaries.

On native Linux or Windows, also run the actual older-updater migration:

```sh
python3 tests/native/monitor-launcher-update-e2e.py --bins "$verify_bins" \
  --fetch-old-version 0.3.52 --out "$verify_evidence/monitor-cutover"
```

Require PASS for the six-core update followed by separate same-version repair,
exact installed tag, immutable metadata and digest checks, ownership refusals,
matching bundle and executable role, locks, retry, and core rollback preserving
auxiliary state. Windows adds sharing-failure recovery. Missing symlink privilege
produces PARTIAL and exit 2. The fixture uses actual executables with a transport
substitute. Prepared-state recovery is seeded from its actual receipt and backup;
it is not a timed kill test. macOS repairs through verified app replacement.

## Verify packaged feed toggles

Inside a clean VM, install the package and verify its physical provenance.
Run the same production checks directly against that installation:

```sh
python3 scripts/test-plugin-runtime.py --bundle "$verify_bundle" \
  --bins "$installed_bins" --frame-probe "$verify_frame_probe" --feature production \
  > "$verify_evidence/packaged-activity.log" 2>&1
```

Set `installed_bins` to the package's installed executable directory. Build the
native `plugin_frame_probe` example separately from the frozen source and set
`verify_frame_probe` to its path. This test instrument reads the daemon's frames;
it is not part of the installed product. `--bins` verifies all five packaged
consumers against the source revision and bundle before driving them. It builds
nothing. Require both identity and runtime PASS lines and exit 0.

Install both plug-ins disabled, inspect their exact versions/digests, and enable
them with only the local activity grant. `pmux-activity-monitor --json` must then
report the feed disabled. Without restarting the daemon, toggle collection on,
off, and on again. Require a page only while enabled and a fresh epoch after
reenabling. Compare daemon PID, session/window/pane/child-PID tuples, literal
pane output, and mailbox records before and after. Do not use an operator's daemon.

Native Windows and Linux package/runtime claims require that OS's clean proof
environment. Cross-compilation and another OS's screenshots do not cover them.
Keep missing runner, display, permission, or provider prerequisites explicit.
