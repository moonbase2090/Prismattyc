# Testing policy

Adopted 2026-09-04 after the spaces push (epic PT-212). Six of the seven bugs
found in review passed the unit suites. They lived at seams: a real child
process, an idle event loop, a subprocess race, or the real paint path. This
policy makes those seams part of the merge gate.

## Gates

Every PR passes these jobs before merge. On the 32 GiB Nexus host, run
`scripts/la-staged-pr.sh`. Do not fire one-shot
`local-actions run --event pull_request`. That path leaves act
containers up and keeps zram full. The staged script runs light jobs,
then mutants, then CRAP, then e2e. It reclaims after each stage. It
checks host headroom before each heavy stage. If headroom still fails
after reclaim, the script exits with `reclaim did not restore
headroom`. Single-job debug still uses
`local-actions run --job NAME`.

Heavy jobs must not overlap on the dogfood host (PT-305): `mutants`,
demo-box jobs (`spaces-e2e`, `spaces-e2e-wayland`,
`walkthrough-caption-e2e`, `render-bench`), and CRAP lcov (`crap`,
`crap-refresh`, `crap-release`). `scripts/la-heavy-serial.py` takes one
exclusive lock for those jobs. Acquire waits and retries when the lock
is held. A wait timeout is infrastructure. It is not a content red.
The lock is a Docker container named `prismattyc-la-heavy`. The owner
token and file-lock fallback live in a user-writable directory:
`${XDG_RUNTIME_DIR}/prismattyc-la` when that runtime dir is set and is
not `/tmp`, otherwise
`${XDG_CACHE_HOME:-$HOME/.cache}/prismattyc/la-heavy`.
Do not use `/tmp/prismattyc-la`. Docker creates that bind as
`root:root` mode 755, and the runner uid cannot write `owner`.
Acquire creates the lock dir mode 1777 when it can. If the dir is
not writable, acquire fails with a PT-305 message. It does not raise
a raw `PermissionError`.
Only the process that received the owner token may release the lock.
A second process in the same job, or another job on the same host,
cannot acquire or release that lock. The acquire step writes the owner
token into that lock dir and to `GITHUB_ENV` when that file is
present. The Release step reads the owner file so a skip path (no
mutatable Rust) still releases with the same token.
Unit tests use a temp lock dir and do not append tokens to
`GITHUB_ENV`.

| Gate | Job | Rule |
| --- | --- | --- |
| Unit | `test`, `lint` | crate tests and `clippy -D warnings` |
| Box e2e | `spaces-e2e` | `demo/spaces-e2e.sh` in the demo box; X11 softbuffer; the box must run, never skip |
| Box e2e caption | `walkthrough-caption-e2e` | `demo/walkthrough-caption-e2e.sh` in the demo box; X11; double-click `[show me]` advances one step and does not skip; never skip the job |
| Box e2e Wayland | `spaces-e2e-wayland` | `demo/spaces-e2e-wayland.sh`; weston headless `--socket=pt290-wayland`; native `wl_shm` ARGB8888 |
| Render bench | `render-bench` | `demo/render-bench.sh` in the demo box; per-frame cells max/mean; archive under `build/render-bench/<version>` |
| CRAP | `crap` | `cargo llvm-cov` + `cargo-crap`; new or newly above-40 functions in PR-touched files block merge; global counts are informational; top 10 per crate |
| Mutation | `mutants` | one crate at a time, with render routing and remainder shards; caught rate at or above 60% per crate when scored ≥ 5; score only the merged full universe; fewer scored are reported, not gated; runner OOM is infrastructure; refuse to start below host headroom; scratch on disk, never tmpfs `/tmp` |

A nightly job runs `cargo-mutants` on every crate and archives the report.

## Validate new work incrementally

Record the last tested code revision and reuse its results when the code
has not changed. Test the new diff and the affected integration paths.
Do not repeat the complete workspace suite solely because documentation,
version stamps, or gate thresholds changed. Repeat a broader suite when a
new failure or a shared-code change justifies it.

For a scoped mutation run, retain every mutant generated from the new diff.
Report its base revision, test selectors, complete denominator, and result.
Label carried-forward evidence separately. A focused run does not establish
a new full-workspace gate pass. Run long validation jobs in the background
and retain their status and logs.

## Route PR mutations

`scripts/mutants-pr.sh` discovers the complete `--in-diff` mutation set
for each touched crate. `scripts/mutants-route.py` runs these steps:

1. Select changed lines inside the functions and modules listed below.
2. List the tests for each selected group. Fail if a selector matches zero
   tests. Discover mutations for each group. Verify each complete identity
   against the original mutation set.
3. Run the ordinary full crate test suite on the unmutated source.
4. Run each routed group in its declared named test phase.
5. Split the non-render remainder into fixed-size shards (default 16
   identities; `MUTANTS_SHARD_SIZE`). Use the same iterate and merge
   contract as the render phases. Overlap partners stay in one shard.
6. Retain validated full-suite misses as missed. Do not run them again in
   the same invocation. Send focused misses and timeouts to the full suite.
7. Retry a full-suite timeout once when source-line selection can isolate it
   without repeating a completed miss. Otherwise retain it as a scored
   timeout. Record the reason in `retry_policy`.
8. Validate every phase report. Replace each focused survivor result with its
   full-suite result. Score each original mutation once. Do not treat a
   mid-run shard rate as the gate result.

The real-window phase applies to these functions in
`crates/prismattyc-host/src/main.rs`:

- `App::paint`
- `PresentBackend::paint`
- `PresentBackend::supports_partial_raster`
- `current_full_repaint_reason`
- `rasterize_frame`

The declared source-to-test mapping is below. All source paths are
relative to `crates/prismattyc-host/src/`.

| Source | Fast test selector | Match |
| --- | --- | --- |
| `space_open.rs` | `space_open::tests::` | Substring |
| `main.rs`: `open_space_from_host`, `advance_space_opens`, `poll_host_attach_tabs`, `persist_attach_selection` | `space_open_window_tests::delayed_chip_opens_keep_cache_label_and_focus_in_order` | Exact |
| The five `main.rs` functions above | `render_window_tests::real_window_paint_reaches_the_backend` | Exact |
| `wayland_shm/buffer_age.rs` | `wayland_shm::buffer_age::tests::` | Substring |
| `frame_damage.rs` | `frame_damage::tests::` | Substring |
| `main.rs`: `framebuffer_scroll_plan`, `apply_framebuffer_scroll_blits` | `tests::framebuffer_scroll_` | Substring |
| `main.rs`: `row_after_scrolls` | `tests::row_after_scrolls_tracks_copied_cursor_pixels` | Exact |

The router runs one fast phase for each non-empty mapping group. It then
runs full-suite remainder shards. Fallback covers identities that still need
a full-suite result and eligible timeout retries. Each validated outcome
records `suite_scope` as `focused` or `full`. A full-suite miss stays missed;
an implicit second-pass catch must not improve its score. An operator may
request a separate rerun in a fresh output directory to investigate flakiness.
Pass `--exact` only for complete test names. Do not use the broad
`tests::` selector, which also matches tests in unrelated modules.

All other functions use the full suite. Extend the fast list only after
you validate a focused test for the added function. See the
[Space mutation route evidence](mutants-space-routes.md) for selector scope
and measurement limits. A mutation whose span
overlaps a changed line remains in the score, even when most of its
function did not change. Record coverage debt without excluding it.

### Spaces P1.1 exclusion exception

PR #349 has an explicit exception to the exclusion rule above. Nexus relayed
the operator's peel/skip decision in PMUX letter #4280 after cancelling
mutation run `1789052743-6354652e`. These seven functions retain their runtime
behavior but are excluded from mutation discovery until their follow-ups land:

| Function | Follow-up |
| --- | --- |
| `App::pump` | #355 |
| `open_space_from_host`, `poll_host_attach_tabs` | #356 |
| `persist_attach_selection`, `save_space_from_host` | #357 |
| `App::publish_render_status` | #358 |
| `Active::drop` | #359 |

The attributes exclude whole functions, including mutations that earlier
tests caught. Do not report excluded identities as caught or equivalent.
The before/after inventory is `docs/evidence/spaces-345-exclusions.json`.
Report the excluded count beside the remaining mutation universe. The 60%
gate applies to the remaining universe; it does not validate the exclusions.
Remove each attribute when its follow-up restores meaningful mutation coverage.
This exception does not authorize exclusions for other functions or PRs.

### Baseline and result accounting

The full unmutated baseline runs even when the fast phase catches every
mutation. Each mutation run uses `--jobs 1` and `--test-threads=1`.
The fallback skips its duplicate baseline after the ordinary full suite
passes. Nightly runs continue to use the full suite for every mutation.

Routing requires cargo-mutants 27.1.0. Its regex filters do not cover
`StructField` mutations. The router therefore selects current source
lines through `fast-*.diff` and verifies discovery. These files are
selection input. Do not apply them as source patches.

The router combines only the current fast-phase name files for `--iterate`.
Completed misses never enter the iterate name files. The router excludes
them with `full-fallback.diff` and verifies the remaining discovery identities.
An identity without a full-suite result that cannot be selected fails the run.
It verifies the names in `caught.txt`, `unviable.txt`, and
`previously_caught.txt` against actual results before reuse. A source or
diff change, missing outcome, duplicate identity, unexpected mutation,
inconsistent count, empty test selection, failed baseline, or failed phase
stops the run.

Read `build/mutants-pr/<crate>/routing.json` for the source digest,
selection counts, and shard list. The same directory contains discovery
JSON, phase logs, matched test names, and separate `fast-*/`, `shard-*/`,
and `full/` reports. `retained_full_misses` counts full misses that do not
repeat. `retained_full_timeouts` counts timeouts that cannot be isolated from
those misses. `full` counts identities actually sent to the final fallback.
The validated combined report is
`mutants.out/outcomes.json`. A routing failure cannot pass the gate
through the small-sample exception.

## Detect mutants runner OOM

The kernel OOM killer usually targets rustc or a test binary, not
`cargo-mutants`. The wrapper then exits 2 (missed) or 4 (baseline
failed). The gate must not treat that as a caught-rate failure.

After each run, read `/sys/fs/cgroup/memory.events` (`oom_kill`) and
compare it with the sample taken before the run. An increment is OOM,
regardless of the exit code. `cargo-mutants` exit 137 is a second
signal (the wrapper itself was killed). Override the events path with
`MUTANTS_OOM_EVENTS`.

Exit policy:

- PR (`scripts/mutants-pr.sh`): OOM fails the job with exit 137. Do not
  keep scoring missed mutants.
- Nightly (`scripts/mutants-nightly.sh`): OOM records a failure for that
  crate and continues so later crates still archive. The job exits
  non-zero.

The job container memory cap is containment (`MUTANTS_MEMORY`, default
`8g`). Keep this cap so the container dies before the 32 GiB host
(PT-305). Do not raise it. The mutants job container also passes
`--init` so pid 1 reaps (PT-259). act's default pid 1 is
`tail -f /dev/null`, which does not reap. Sample from #306 mutants
`1788715639-d15f47f3`: HostConfig.Init was null; 76 defunct processes
(sleep and xkbcomp) had ppid 1. Those zombies are not the 8g RSS. Every
Local Actions job that sets `container.options` also passes `--init`.
That field replaces act's `--container-options`. Without `--init` here,
the reaper is dropped.

The PR gate also refuses to start when the host is already under
pressure. Require at least 10 GiB `MemAvailable` and swap use below 50%
(`MUTANTS_MIN_FREE_RAM`, `MUTANTS_MAX_SWAP_RATIO`). The job reads host
`/proc/meminfo`. When the 8g cgroup hides the host view, it uses a
pid-host Docker sidecar. Set `MUTANTS_HOST_MEMINFO` to inject a file.
A failed floor check is infrastructure. It is not a caught-rate failure.
On Nexus, swap at or near 100% makes this refuse expected until swap
drains. That is host pressure, not a gate bug.

Put cargo-mutants scratch on disk. On the host, use
`${XDG_CACHE_HOME:-$HOME/.cache}/prismattyc/mutants`. Do not use `/tmp`.
`/tmp` is tmpfs on the dogfood host and fills with multi-GiB
`cargo-mutants-*` leftovers.

The gate refuses TMPDIR when the path is under `/tmp` or when
`findmnt -no FSTYPE` reports `tmpfs` for that path. A path other than
`/tmp` that still sits on tmpfs also fails.

The Local Actions `mutants` job bind-mounts host
`/var/cache/prismattyc/mutants` at `/cache/prismattyc/mutants`. It sets
`MUTANTS_TMPDIR` and `TMPDIR` to the container path. act cannot
interpolate env into `container.options`, so the host path is fixed.
Create `/var/cache/prismattyc/mutants` on SSD. You may symlink that
directory to `${XDG_CACHE_HOME:-$HOME/.cache}/prismattyc/mutants`.

## Run Local Actions in stages on Nexus

Use `scripts/la-staged-pr.sh` on the 32 GiB host. The stages are
`light`, `mutants`, `crap`, and `e2e`. `phase3-rich` stays in
`ci.yml` for hand and nightly runs. It is not a required light-stage
job. `local-actions run --job` returns when the job is queued
(exit 0). That is not a pass. The script waits on
`local-actions status <id>` until the job is `succeeded`, `failed`,
`cancelled`, or `lost`. Only `succeeded` with `exit_code: 0` is a
pass. The script then reclaims. Do not reclaim while a stage job is
still queued or running.

`scripts/la-reclaim-host.sh` stops leftover `act-*` containers,
drops `prismattyc-la-heavy` when no other heavy job remains, and
deletes `cargo-mutants-*` under the SSD cache paths. It does not
clear host `/tmp`.

Zram reclaim needs passwordless root. Nexus prompts for a password
today, so reclaim cannot run from the agent.

1. Copy `scripts/la-reclaim-zram.sh` to
   `/usr/local/sbin/prismattyc-la-reclaim-zram`.
2. Keep that file root-owned and executable.
3. Add a sudoers drop-in that allows only that path. Nexus is Arch
   Linux. Brandan is in group `wheel`, not `sudo`. Use `%wheel`:

```
# /etc/sudoers.d/prismattyc-la-reclaim
Cmnd_Alias PRISM_ZRAM = /usr/local/sbin/prismattyc-la-reclaim-zram
%wheel ALL=(root) NOPASSWD: PRISM_ZRAM
```

You may use an equivalent polkit rule. Do not grant passwordless sudo
for arbitrary commands. Test with
`sudo -n /usr/local/sbin/prismattyc-la-reclaim-zram --dry-run`.

Pass `--allow-missing-zram` only on hosts that have no zram. On Nexus,
install the rule. The staged script then reclaims swap between stages
so the next headroom check can pass.

A bounded `cargo mutants --jobs 1 -p prismattyc-host --file icon.rs`
run (5 mutants, full host suite) measured process-tree VmRSS:

- compile: 4.56 GiB
- rust-lld link: 1.31 GiB
- test execution (up to 40 processes, rustc absent): 0.23 GiB

Compile is the peak. The ticket's worry that 8g would cause the PT-255
137s does not hold for `--jobs 1`. Those 137s were unconstrained
parallel jobs on the 31g host. The cap is not an OOM fix. Host and mux
together in one `--in-diff` hit 8g (PT-286). The PR gate runs one crate
at a time and shards the remainder. Do not raise the cap to combine
crates. A typical Spaces-sized host diff must complete on Nexus without
a host swap death spiral.

Act cannot interpolate env into `container.options`, so the YAML starts
at 8g and the job applies `MUTANTS_MEMORY` with `docker update`, then
reads `memory.max` and fails if it does not match. Raise the env if a
baseline compile is killed at 8g with `oom_kill` incrementing.

`gherkin-mutator` acceptance mutation stays for Termwright features
(`scripts/acceptance-mutate.sh`).

## Check CRAP before merge

The `crap` job compares each function in a PR-touched file with
[the committed baseline](crap-baseline.json). It blocks merge in two cases:

- A new function in a touched file has CRAP above 40.
- A function in a touched file moves from CRAP at or below 40 to above 40.

The ordinary merge job scans the whole touched file. An incremental run
may score only the changed functions when it records its scope and carries
forward the prior evidence. A function already above 40 may remain above 40. A lower global count does not excuse
a new or newly above-threshold function in a touched file.

The global count may grow without blocking the PR. The report prints the
baseline count, current count, previous-release count, reduction target,
and top 10 functions above 40 per crate. The comparisons with the baseline
and previous-release counts are informational. The job must obtain the
complete changed-file list. Missing git metadata is a failure.

## Check CRAP before a tagged release

Run this check from the release candidate checkout before you cut a tag:

```bash
local-actions run --event workflow_dispatch --job crap-release
```

The Local Actions daemon must use that checkout as its root. Require
`status: succeeded` and `exit_code: 0` for the returned run ID before you
publish the tag. A per-merge `0.1.x` version stamp does not run this check.

[The release workflow](../.github/workflows/crap-release.yml) captures
coverage in the runner and invokes `scripts/crap-gate.py --release`.
It requires at least 10 fewer functions above CRAP 40 than the last
refreshed baseline. The target stops at zero. One report line prints the
current count, last refreshed baseline count, actual delta, and target.
The regular `crap` job continues to enforce the file-scoped merge checks.

The comparison source is the last deliberately refreshed baseline. It
does not track each workspace version bump or update at merge time.
The release check counts that baseline's entries. The `previous_release`
and `previous_above_count` fields describe the earlier refresh and remain
visible for context. For example, a baseline count of 181 gives a release
target of 171, even if the workspace version has moved since the refresh.

Keep [the refresh script](../scripts/crap-refresh.sh) as the deliberate
update path. After an intentional cleanup, run
`local-actions run --job crap-refresh` and review the resulting baseline
before you commit it. A host coverage capture is not comparable. Do not
refresh merely to move the release target past a failing candidate.

## Read a high CRAP score before you split

CRAP at coverage 0 is C²+C. An exhaustive match over N enum variants is
cyclomatic N or N+1. That score is not a design problem by itself.

Classify a hot function before you extract:

1. **Name map.** Pure match. No I/O. Each arm returns a label or grouping.
   Table-test every variant. Keep the match in one place. The compiler
   exhaustive match is the ratchet when the enum grows.
2. **Dispatcher.** Match or branch that does I/O, mutation, or process work
   inline. Extract a pure decision only when the arm chooses among outcomes.
   Do not split a function whose only cost is being exhaustive.

`control_event_kind` is a name map (22 `Event` arms). Table tests drop it
from CRAP 552 to about 23. `cmd_detach_other` and `run_update` are
dispatchers. Send a classification with the arithmetic before you cut those.

CRAP is C²(1 − cov)³ + C. The trailing +C is a floor. A function cannot
score below its cyclomatic complexity. Coverage cannot bring a function
with C above 40 under the threshold.

When C is above 40, either accept the function as a permanent resident of
the above-40 count, or change its structure for reasons that stand on
their own. Do not split it to chase the number.

The release −10 target cannot reach zero while any such residents remain.
Count the functions that are above 40 and have C above 40. That count is
the reachable floor.

## Seam rules

1. **One box step per claimed effect.** A ticket that changes what the
   user sees or does ships with a step in `demo/spaces-e2e.sh` (or a
   sibling script). A ticket that claims a behaviour or performance
   change does too. The step is the acceptance criterion. The reviewer
   runs it. A performance ticket is not exempt because it changes no
   pixels. Docs-only, CI-only, and claimed no-op refactors write "no
   behaviour or performance claim" on the PR.
2. **No helper steps around the action under test.** After the action:
   wait, then assert. A test that needs a pointer wiggle, an extra CLI call,
   or a second click to pass has found a bug. File it; do not add the helper.
   `WIGGLE` defaults off.
3. **Real children.** A host test that models "the user runs X in a pane"
   spawns the real binary (`pmux-attach`, a script that emits OSC titles).
   Fake argv is not a seam test.
4. **Idle before assert.** Host-state assertions (cache file, rail, title)
   run after at least 2 s with no input. An idle host must keep working.
5. **Paint through the real path.** Overlay and chrome tests rasterize the
   real frame onto a gradient backdrop with a translucent config, then
   assert pixels. Painting a helper onto a bare buffer is not a rendering
   test.
6. **Reviewer box run.** The reviewer builds the PR, runs it in the demo box
   (`docker create --init`, `docker cp` the bins), drives the feature, and
   attaches screenshots to the PR. A PR that changes pixels is not merged
   without them.
7. **Assert the mechanism, not the absence of breakage.** "The app still
   works" is not evidence that the change did anything. Name one
   observation that would change if you reverted the change to a no-op.
   That observation is the step. It may be a counter, a log field, a
   timing bound, a pixel, a title, a cache field, or a protocol
   message. Recording a number without a bound is not an assertion.
   If you cannot name a failing observation, the box run cannot prove
   the claim.

## Worked example: PT-243

Partial raster shipped with a config flag that defaulted to true and
short-circuited the optimization on every frame. 594 unit tests and
38 box checks passed. None of them observed a partial frame. The
feature never ran.

The step that caught it asserts the mechanism: after a small screen
update, `full_repaint_reason=-` and `cells_painted` is below the full
grid. That step fails if partial raster stops engaging. The earlier
checks would still pass.

Most box steps cannot be a single counter. The no-op question is the
test that still applies. Title counts, cache fields, loaded-fallback
log lines, and grapheme exports already fail if you revert those
changes. A counter is one shape of that test, not the only shape.

## PR template

`.github/pull_request_template.md` asks for: what changed, how it was
verified (unit, e2e step, box run, screenshots), the no-op test (what
the step would catch if the change were a no-op), the CRAP delta, the
mutants score, and which seams the change touches. If the no-op answer
is nothing, the step is wrong.

## Why

PT-210 (nested attach), PT-171 (idle poll), PT-218 (chip-click race),
PT-220 (flat overlay), PT-221 (title revert), and the first PT-213 pass all
passed unit tests. Each was found by driving a real host in the box.
PT-243 passed 594 unit tests and 38 box checks while partial raster
never ran. The box run only became evidence when a step asserted the
mechanism.

## Run host render tests with a real window

Linux host tests require `Xvfb` and the X11 client libraries that winit loads.
On Debian or Ubuntu, run `./scripts/install-render-test-deps.sh` to install them.
On Arch Linux, install `xorg-server-xvfb`, `libxcursor`, `libxi`, `libxrandr`,
and `libxkbcommon-x11`. A missing executable or library fails the test.

The render fixture creates a real winit window and a softbuffer backend.
Each invocation starts Xvfb with `-displayfd`. Xvfb allocates an unused display.
The fixture runs its window event loop in a child test process. The child uses
private configuration, state, runtime, and mux socket paths. It does not need
`DISPLAY` or `WAYLAND_DISPLAY` in the parent environment.

Run the focused fixture:

```bash
cargo test --locked -p prismattyc-host render_window_tests:: -- --nocapture
```

The fixture checks framebuffer pixels, overlay removal, IME preedit,
background caching, pane damage, and paint bookkeeping. It injects one
presentation error to check backend restoration and retry. All successful
presentation checks use the real backend. The X11 fixture does not prove
Wayland or GPU presentation.

The test owns and reaps its display and child process on success or panic.
Display startup and child execution have bounded deadlines. A deadline failure
fails the test. The parent also requires a completion marker written after all
window assertions. Selecting zero child tests is a failure, even when the
child exits successfully. The test prints startup, window creation, and paint times.

Coverage and mutation jobs install Xvfb too. Keep mutation baselines enabled.
A failed or timed-out baseline must fail the gate. Choose a suite timeout from
an observed baseline; the duration of the focused window test does not measure
the full host suite.

## Present backends (PT-290)

The owner runs KDE Wayland (KWin). The demo box has two e2e jobs on one
image:

| Job | Present path | What it proves | What it does not prove |
| --- | --- | --- | --- |
| `spaces-e2e` | X11 softbuffer via Xvfb + Openbox | X11 layout, xdotool steps, PT-243 partial raster | Native `wl_shm` |
| `walkthrough-caption-e2e` | X11 softbuffer via Xvfb + Openbox | Double-click `[show me]` advances the caption by one step and does not skip (PT-295 / PT-297) | Native `wl_shm` |
| `spaces-e2e-wayland` run 1 | weston headless `--socket=pt290-wayland`, `window_opacity = 0.95`, host `wl_shm` ARGB8888 | `wl_shm`, buffer rotation, and our damage handling against a conforming compositor | KWin buffer-release timing |
| `spaces-e2e-wayland` run 2 | same weston socket, `window_opacity = 1.0`, `window_blur = false`, host softbuffer | Opaque native Wayland does not take `wl_shm`; splash pixels are gone across two presents after dismiss and type (PT-294) | KWin buffer-release timing |

Do not treat the Wayland job as owner-KWin equivalence. Buffer-release
timing and how long a compositor holds a buffer are what buffer-age
code depends on. A KWin-specific difference is the class that job
cannot catch. The owner machine is the final check for that class.

The job uses weston headless `--socket=pt290-wayland`. That is the
socket this job creates. No `--privileged` (PT-278).

### Env contracts

X11 job (unchanged):

- `DISPLAY=:99`
- `WINIT_UNIX_BACKEND=x11`
- `WAYLAND_DISPLAY` unset

Wayland job:

- `WAYLAND_DISPLAY=pt290-wayland` (unique socket this job creates)
- `XDG_RUNTIME_DIR` set
- `DISPLAY` unset
- `WINIT_UNIX_BACKEND` unset
- never `WINIT_UNIX_BACKEND=x11`

The first Wayland run must set `window_opacity` below 1. The host
takes the `wl_shm` path only when `want_alpha` is true. The second run
sets `window_opacity = 1.0` and `window_blur = false`. An opaque
config on native Wayland stays on softbuffer.

### Mechanism assert

The first step greps the host log for
`prismattyc-host: wayland shm present (ARGB8888)`. Fail if weston does
not publish `$XDG_RUNTIME_DIR/pt290-wayland`, if that line is missing,
or if the log contains `falling back to softbuffer`. Do not require
` + blur`. A nested XWayland window that looks fine is a fail.

The second run fails if that `wayland shm present` line appears, if
`PRISMATTYC_DUMP_PRESENT` has no PNG after present, if the splash
signature is missing after launch, or if splash-coloured pixels from
that first capture remain after Return and type across two presents.
The dump is the exact CPU slice handed to `present` on that frame,
not a second raster. The dump path is captured once at window create.
A sidecar JSON next to the PNG records `full`, `full_repaint_reason`,
and a monotonic `seq`. The opaque step waits for `seq2 > seq1` so
present1 and present2 are distinct presents. Splash signature pixels are SPECTRUM facet colors with SLOP 12
(tinted beam), not INK, so anti-aliased grey edges are excluded.
`MAX_STALE` is 0. weston has no virtual keyboard protocol,
so the job sets `PRISMATTYC_E2E_DISMISS_SPLASH_MS` (off by default).
After N ms the host feeds one Enter through `dispatch_splash_key`,
the same function `WindowEvent::KeyboardInput` uses, which calls
`splash::key_action`. Typed text goes through `pmux send` (PT-126)
into the real session PTY. X11 `spaces-e2e` still uses xdotool. Do
not wait on grim or weston-screenshooter. Do not skip.

A stale `prismattyc-demo` image may lack weston, grim, or wtype.
`demo/docker/run.sh spaces-e2e-wayland` installs those three with
pacman when any is missing.

`demo/docker/run.sh spaces-e2e` stays X11.
`demo/docker/run.sh spaces-e2e-wayland` starts weston
`--socket=pt290-wayland` in the same `prismattyc-demo` image.
