---
name: verify-prismattyc
description: Verify Prismattyc desktop UI, pmux sessions and mail, saved spaces, and Windows packaging when preparing PR proof or checking runtime behavior.
---

# Verify Prismattyc

Read [the feature map](features/README.md), then select the entries affected by the PR. Record the tested commit, OS, presenter, commands, and results. Follow [REVIEW_POLICY.md](../../../REVIEW_POLICY.md) and [testing policy](../../../docs/testing-policy.md) for required gates; this skill supplies runtime proof, not a replacement for those gates.

## Launch

Run from the repository root. Python 3.8+ and a Unix shell are required for the CLI helper. Rust 1.90.0 is the CI toolchain. Build all binaries together so the host, CLI, and daemon belong to the same revision:

```sh
verify_repo=$(pwd)
verify_target=$(mktemp -d /tmp/prismattyc-target-XXXXXX)
export CARGO_TARGET_DIR="$verify_target"
cargo +1.90.0 build --workspace --bins --locked
verify_bins="$verify_target/debug"
verify_evidence=$(mktemp -d /tmp/prismattyc-proof-XXXXXX)
git rev-parse HEAD > "$verify_evidence/revision.txt"
git diff --stat > "$verify_evidence/worktree.txt"
uname -sm > "$verify_evidence/platform.txt"
```

Use an installed compatible compiler and SDK on macOS. If the active linker rejects SDK `.tbd` targets, select a matching Xcode/Command Line Tools installation or an already-installed compatible `SDKROOT`; preserve that choice in the build log. Do not prescribe an SDK path that does not exist on the test machine. On Linux, install the native dependencies in `scripts/install-render-test-deps.sh` before driving X11 windows.

For short-lived CLI drives, the helper below starts a fresh private `pmuxd` for each feature and waits for its Unix socket. It clears inherited mux/application variables and isolates HOME, configuration, data, state, and runtime paths. A disposable shell wrapper bypasses login startup files, so they cannot reset the isolated HOME or prompt. It owns teardown. Run the two drives serially:

```sh
.cursor/skills/verify-prismattyc/scripts/verify-cli.py \
  --bins "$verify_bins" --out "$verify_evidence/sessions-mail" --feature sessions-mail
.cursor/skills/verify-prismattyc/scripts/verify-cli.py \
  --bins "$verify_bins" --out "$verify_evidence/spaces" --feature spaces
```

Each `--out` must be new. A successful drive prints `"status": "PASS"`. For desktop UI, use the native fixture in [desktop](features/desktop.md); it owns separate host and daemon processes. For Windows use [Windows](features/windows.md), not this Unix launch recipe.

## CI and test portability

For changes to process-detection fixtures or Linux test runners, follow the
[session and mail portability checks](features/sessions-mail.md#linux-test-portability).
The Ubuntu 26.04 preview job in `.github/workflows/ci.yml` is advisory
(`continue-on-error: true`); record its actual result separately from required
CI. A green aggregate does not establish that the preview suite passed.

## Doctor

The CLI helper performs a read-only doctor before the drive, after success, and after a failed drive when the daemon is still alive: check its owned process, check the private socket, run that build's `pmux --version` and `pmux ls`. Inspect `doctor.jsonl` and compare the version with `revision.txt`; a response from an unrelated installed binary is not proof.

For a desktop fixture, inspect its process records/logs and query `pmux render-status --json` under **that fixture's environment**. Require the returned `host_pid` to equal the owned host PID, the intended space, and a recent successful frame. Use `kill -0` on the recorded PID on Unix. The CLI helper's temporary daemon no longer exists after it returns; do not query its deleted socket.

After surprising output, stop driving until doctor passes. A healthy process with a stuck overlay needs a reset or fresh isolated launch. Preserve failure artifacts and clean the failed run before retrying. Report a missing OS, display, Accessibility permission, SDK, or package as an unmet prerequisite with the attempted command.

## Drive

Use the exact recipes in the feature files. The CLI helper invokes production `pmux` commands and uses a read-only protocol snapshot to check session identity. Native keyboard/mouse events prove desktop actions; a CLI mutation alone does not prove that a UI menu or shortcut works. If several mapped entry points changed, cover each affected entry point.

## Evidence

Keep artifacts under `$verify_evidence`, outside disposable runtime state. The CLI helper writes `commands.jsonl` with arguments, stdout, stderr, and exit codes; `doctor.jsonl`; `pane.txt`; `daemon.log`; and `result.json`. The space drive also copies saved definitions before deletion.

UI proof includes the action or open overlay and the resulting screen, plus a read-only check of the changed state. `PRISMATTYC_DUMP_PRESENT` captures the frame passed to presentation, not desktop composition; label it accordingly. Capture OS screenshots when proving titlebar, taskbar, installer, or compositor behavior. Inspect images before publishing and use neutral session names and shell prompts.

Put artifact links and actual assertions/results in the PR Proof section. Preserve failures and distinguish retries from a clean first pass. A test selector that runs zero tests is not proof. Do not equate mocks, internal state setters, Linux rendering, or macOS captures with Windows runtime coverage. For a dry-run, inspect resulting files/processes/network or refs to establish what it actually skipped.

## Cleanup

The CLI helper terminates and waits for only its own daemon, removes its scratch tree, and retains its evidence directory on success or failure. Confirm `daemon_reaped` and `scratch_removed` in `result.json`, then confirm `commands.jsonl` and `pane.txt` still exist after a successful run. Native fixtures own their teardown too; check their final status and inspect residual recorded PIDs if interrupted. Never use process-name kills or stop the user's mux.

After all drives and any re-proofs, remove only the build directory created in Launch if it is no longer needed. Keep `$verify_evidence` until the reviewer can access the proof. Evidence can contain shell output or paths; inspect it before uploading.

## Helpers

`scripts/verify-cli.py` is executable; its two invocations appear in Launch. It requires no third-party Python packages and performs no installer or updater actions. Existing native and packaging scripts are linked in the map instead of duplicated here.

Use `/maintain-verification-skill` when a PR changes how the application builds, runs, or is tested. Its maintenance pass must exercise every mapped feature, or report the concrete platform/prerequisite that blocked coverage.
