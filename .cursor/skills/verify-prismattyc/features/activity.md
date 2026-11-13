# Agent activity and contained plug-ins

The daemon owns the headless activity ring. Collection is on by default.
`[mux] experimental_activity = false` disables it; `--experimental-activity`
enables it. Configuring both `activity_webhook_url` and
`activity_webhook_secret_file` also enables collection. Config changes start or stop collection without restarting the
daemon or replacing sessions. See [the feed design](../../../../docs/agent-activity-feed.md).

The monitor UI and connector query logic run as contained plug-ins. The native
`pmux-activity-monitor` executable presents validated frames and forwards its
compatibility query modes. First use installs and enables the bundled defaults
with local activity read for the connector and local read plus stopped-session
cleanup for the monitor when no saved package selection exists. Saved disables
are preserved. Explicit package installation leaves the package disabled.
`PRISMATTYC_DEFAULT_PLUGINS=0` skips default provisioning for isolated tests.
Endpoint registration, package grants, and the MCP operator allowlist are
separate steps.

## Drive mouse input through the native host

After Launch, run the coordinate boundary replay and both monitor flows:

```sh
python3 tests/native/activity-monitor-flow-e2e_test.py
python3 tests/native/activity-monitor-flow-e2e.py --no-gui \
  --bins "$verify_bins" --out "$verify_evidence/activity-pane"
python3 tests/native/activity-monitor-flow-e2e.py \
  --bins "$verify_bins" --out "$verify_evidence/activity-native"
```

Require `PASS`, `result.json`, and empty `leftovers_after_teardown` for each
flow. The native open step must report active SGR mouse tracking, and quit must
report `mouse_capture_restored: true` before reopening. Windows views register
capture through scoped console input flags as well as terminal sequences.
Inspect the native mouse step for click selection, second-click details,
wheel selection and return, and click-to-dismiss help. Plain right-click opens
the host pane menu; Shift+right-click passes through to return to the monitor's
list. The terminal-only flow sends SGR mouse reports directly to the pane and
does not prove host input routing.

The native drive requires an allocated interactive desktop. macOS needs
Accessibility permission; Linux needs X11, XTEST, xdotool, and a window manager;
Windows needs an interactive user session. The fixture owns private daemon,
host, and synthetic activity state. It does not prove provider hook execution.
Match the recorded revision and binary build before claiming exact-head proof.
Retain `native-input.json` with the native flow result. It records injected
event flags or the registered native window and actual pointer target. Compare
these receipts with render status's `input_state` and pane mouse modes to
distinguish input targeting from guest behavior.
The Linux driver reads the client origin directly from X11; window-manager
decorations must not offset the terminal cell coordinates.
Map daemon pane IDs through `current_panes` to the host's
`visible_pane_content_rects`. `visible_pane_slots` includes pane headers and
padding, so it cannot locate terminal cells.

The mouse event belongs to `pmux:plugin@2.0.0` and `pmux:view@2.0.0`.
These contract versions are independent of package versions. Read the reviewed
bundle tag from `plugins/bundle-pin.json` and package versions from the verified
bundle's `pmux-plugins.json`. The monitor's stopped-session cleanup requires
`activity.manage:local`; the connector remains read-only.

## Build and drive the actual packages

The native Activity Monitor flow builds the matching release executables into
one target directory, checks that `pmux`, `pmuxd`, `prismattyc-host`, and
`pmux-activity-monitor` exist there, then passes that directory to
`tests/native/activity-monitor-flow-e2e.py`. Run the no-desktop pane-key flow
before the GUI flow and activity bench; skip those drives if build or binary
verification fails so missing executables are not reported as flow failures.
CI runs this flow on Linux, macOS, and Windows with the official bundle.

Before preparing a release branch or core tag, run
`python3 scripts/release/prepare-plugin-bundle.py` without `--candidate` and
require exit 0. A missing official pin requires the owner to publish the first
catalog and review its generated pin; follow `plugins/README.md`. CI candidate
proof does not satisfy this release prerequisite. Native CI fetches the same
official pin as public releases. Use the explicit candidate workflow when
exercising unpublished guest changes. Release branches reject a missing
official pin before the shared bundle build starts.

Use Launch to prepare a verified production bundle and build matched binaries.
Then run the production driver, preserving its output outside disposable state:

```sh
python3 scripts/test-plugin-runtime.py --bundle "$verify_bundle" \
  --target-dir "$verify_target" --feature production \
  --out "$verify_evidence/activity-production" \
  > "$verify_evidence/contained-activity.log" 2>&1
```

Require exit 0 and its final PASS. The driver verifies exact archive digests,
empty initial store, fresh-daemon default provisioning of the newest bundled
packages, and actual monitor cells and RGBA tiles. Require monitor local read
plus stopped-session cleanup grants, connector read-only grants and queries,
preserved disables after restart, idempotent reinstall, direct and supervised activity-only MCP,
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

## Verify terminal Escape disambiguation

After Launch, drive the shared Unix presenter through a real private PTY:

```sh
.cursor/skills/verify-prismattyc/scripts/verify-terminal-input.py \
  --bins "$verify_bins" --out "$verify_evidence/terminal-input"
```

Require `status: PASS`, `daemon_reaped`, `scratch_removed`, and restored terminal
modes in `result.json`. Inspect the saved monitor screens and byte schedule.
The drive opens details for `000-beta`, sends lone Escape with the stream still
open, and requires the list within 500 ms. It then sends Escape and `[A` in
separate writes less than 40 ms apart and opens details for `000-alpha`.
It also dismisses help and details using lone Escape, then quits normally.
This tests the actual presenter, contained guest, and terminal emulator.
It does not drive native desktop Escape routing or the separate attach parser.
Windows uses its console event reader and the native activity flow above.

Run the reader regressions as well:

```sh
cargo +1.90.0 test --locked -p prismattyc-mux --lib \
  plugins::presenter::tests:: -- --test-threads=1
```

These use real open Unix streams to check split arrows, Alt modifiers, bounded
lone Escape, and idle bracketed paste. The monitor drive cannot distinguish Alt
modifiers because several monitor actions ignore them. Keep the reader's
modifier assertion when changing this input path.

## Edit hook settings with bounded reads

The file-editing fixture runs independently of daemon collection and provider
execution. Build `pmux` from this revision, then run:

```sh
python3 tests/native/hook-settings-e2e.py --pmux "$verify_bins/pmux" \
  --out "$verify_evidence/hook-settings" --revision "$(git rev-parse HEAD)"
```

Require `status: PASS` in `result.json` and inspect `commands.json`. The fixture
uses private profile/configuration paths and starts no daemon. It checks repeated
installation and removal, unmanaged settings, and complete profile-shape
restoration after reinstalling with a different seat and disabling the failure
hook. Require absent fields to disappear and preexisting empty objects and
arrays to remain. It also checks exact 1 MiB acceptance, oversized
refusal before UTF-8 decoding without changing the original bytes, and malformed
input preservation. Ownership receipts must stay unchanged on identical
reinstallation, and malformed or oversized receipts must preserve both files.
It also checks oversized Claude, Codex, Cursor, and Kiro
profiles through `--install-hooks --all-harnesses`. Unix checks receipt identity
through symlink aliases and target-permission preservation;
Windows omits that case. This drive proves file editing, not provider execution
or contained-package provenance. It requires Python 3, a matching CLI binary,
and a new output directory.

Run the focused boundary and installer tests too:

```sh
cargo +1.90.0 test -p prismattyc-mux --lib --locked config::tests:: -- --test-threads=1
cargo +1.90.0 test -p prismattyc-mux --bin pmux --locked activity_emit::install:: -- --test-threads=1
```

The byte-consumption test must stop after the read limit plus one byte. The
transaction tests cover oversized and small external edits during the
compare-before-replace check, preserving external bytes and cleaning temporary
replacements. Both reads use the hook installer's limit. Other configuration
callers retain their existing unrestricted reads.

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

## Verify webhook delivery locally

Run the real-daemon integration fixture against its local receiver:

```sh
cargo +1.90.0 test -p prismattyc-mux --test activity_webhook --locked -- --test-threads=1
```

Require four passing tests for delivery headers and payload, retry responses,
hook precedence, and session projection. The fixture owns its private daemons
and receiver and requires Unix. It does not contact the configured production
receiver or prove that receiver's deployment. Cryptographic signature checks
are separate unit tests in `src/activity_webhook.rs`.

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

For graph-style changes, exercise the actual candidate component and launcher:

```sh
python3 scripts/test-plugin-runtime.py --feature graph-styles \
  --target-dir "$verify_target" --out "$verify_evidence/graph-styles" --debug
```

Require its final PASS and `graph-styles-result.json`. Inspect the saved text
frames and command records for default braille, explicit braille/block/ASCII,
the persistent `[mux] activity_graph_style`, CLI precedence, known-ASCII
`TERM=dumb` fallback, invalid-config refusal, and retained typed pixel uploads.
Unknown terminal/font coverage must keep braille. This mode builds unpublished
candidate guests; it does not prove the official catalog pin or replace native
Windows, macOS, and Linux screenshots of the changed lanes.
For another OS, pass `--fixture-bundle` pointing at the first run's `bundle`
directory. The driver verifies every archive's exact size and SHA-256 before
building native executables, so each platform exercises the same guest bytes.
This option does not grant production provenance to private fixture packages.

The complete Settings panel also exposes **Multiplexer > Activity graph style**.
Run `tests/native/macos-settings-e2e.py` from [desktop](desktop.md#complete-settings-and-fonts)
with `--feature graph-styles` for a focused native drive. It starts with the
complete panel enabled and cycles block, ASCII, and braille with native keys, checks each saved value,
then resets to the inherited braille default. Relaunch the monitor to apply it;
changing this choice does not restart the daemon or an existing monitor.

For native screenshots on an allocated desktop, start a controlled live seat:
The supplied binary directory must include the matched `pmux-attach` frontend
as well as `pmux`, `pmuxd`, and `pmux-activity-monitor`.

```sh
python3 tests/native/activity-graph-seat.py --bins "$verify_bins" \
  --out "$verify_evidence/graph-seat" --seconds 600
```

Wait for `ready.json`, then run the generated `graph-seat/attach.py` with Python
in a native terminal on that same machine. It supplies the isolated environment
and the matching binary. The default view shows synthetic sample history with neutral
labels. Capture only the owned seat window, inspect braille glyphs and colors,
and exclude prompts, paths, usernames, and machine names. Keep `ready.json`
private because it contains runtime paths. Text frames are not screenshots.
Create `graph-seat/stop` after capture. Require `result.json` to report PASS,
`daemon_reaped`, and `scratch_removed`. The deadline and interruption handlers
also clean the private daemon and scratch directory.

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
auxiliary state. On Windows, retain the workspace-built
`prismattyc-cmd-forwarder.exe` beside the seven executables. Require sharing-failure
recovery, restoration of the missing monitor command in an existing `cmd`
layout, actual command execution through PATH, and restoration on current-frontend
replay. Neither `--check` path may create the command. Missing symlink privilege
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
the monitor with local read and stopped-session cleanup grants and the connector
with local read only. `pmux-activity-monitor --json` must then
report the feed disabled. Without restarting the daemon, toggle collection on,
off, and on again. Require a page only while enabled and a fresh epoch after
reenabling. Compare daemon PID, session/window/pane/child-PID tuples, literal
pane output, and mailbox records before and after. Do not use an operator's daemon.

Native Windows and Linux package/runtime claims require that OS's clean proof
environment. Cross-compilation and another OS's screenshots do not cover them.
Keep missing runner, display, permission, or provider prerequisites explicit.
