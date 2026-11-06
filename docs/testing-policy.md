# Testing policy

Tests cover process lifetimes, event-loop timing, subprocess races, and
rendered output as well as individual functions.

## Gates

Every PR passes these jobs before merge. On the 32 GiB Linux host, run
`scripts/la-staged-pr.sh`. Do not fire one-shot
`local-actions run --event pull_request`. That path leaves act
containers up and keeps zram full. The staged script runs light jobs,
then e2e. It reclaims after each stage. It
checks host headroom before each heavy stage. If headroom still fails
after reclaim, the script exits with `reclaim did not restore
headroom`. Single-job debug still uses
`local-actions run --job NAME`.

Heavy jobs must not overlap on one local machine: the
native-container jobs (`spaces-e2e`, `spaces-e2e-wayland`,
`walkthrough-caption-e2e`, `render-bench`). `scripts/la-heavy-serial.py` takes one
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
| Box e2e | `spaces-e2e` | `tests/native/spaces-e2e.sh` in the native test container; X11 softbuffer; the box must run, never skip |
| Box e2e caption | `walkthrough-caption-e2e` | `tests/native/walkthrough-caption-e2e.sh` in the native test container; X11; double-click `[show me]` advances one step and does not skip; never skip the job |
| Box e2e Wayland | `spaces-e2e-wayland` | `tests/native/spaces-e2e-wayland.sh`; weston headless `--socket=pt290-wayland`; native `wl_shm` ARGB8888 |
| Render bench | `render-bench` | `tests/native/render-bench.sh` in the native test container; per-frame cells max/mean; archive under `build/render-bench/<version>` |
CRAP and mutation are not merge gates. A new test must be able to fail:
break the code it covers and watch it go red, and say so in the PR. The
proof in the PR (commands run and their output, or a screenshot from a
clean VM of the relevant OS) and the end-to-end flows carry the weight.
[Mutation testing](mutation-testing.md) describes a manual pass for
critical crates; it is an aid, not a gate. Scorecard still reports CRAP as
advice.

## Validate new work incrementally

Record the last tested code revision and reuse its results when the code
has not changed. Test the new diff and the affected integration paths.
Do not repeat the complete workspace suite solely because documentation,
version stamps, or gate thresholds changed. Repeat a broader suite when a
new failure or a shared-code change justifies it.

## Run Local Actions in stages

Use `scripts/la-staged-pr.sh` on the 32 GiB host. The stages are
`light` and `e2e`. `phase3-rich` stays in
`ci.yml` for hand and nightly runs. It is not a required light-stage
job. `local-actions run --job` returns when the job is queued
(exit 0). That is not a pass. The script waits on
`local-actions status <id>` until the job is `succeeded`, `failed`,
`cancelled`, or `lost`. Only `succeeded` with `exit_code: 0` is a
pass. The script then reclaims. Do not reclaim while a stage job is
still queued or running.

`scripts/la-reclaim-host.sh` stops leftover `act-*` containers,
drops `prismattyc-la-heavy` when no other heavy job remains. It does not
clear host `/tmp`.

Zram reclaim needs passwordless access to a root-owned helper. Configure
that helper before you run automatic reclaim.

1. Copy `scripts/la-reclaim-zram.sh` to
   `/usr/local/sbin/prismattyc-la-reclaim-zram`.
2. Keep that file root-owned and executable.
3. Add a sudoers drop-in that allows only that path. This example uses
   the `wheel` group. Use your local administrator group:

```
# /etc/sudoers.d/prismattyc-la-reclaim
Cmnd_Alias PRISM_ZRAM = /usr/local/sbin/prismattyc-la-reclaim-zram
%wheel ALL=(root) NOPASSWD: PRISM_ZRAM
```

You may use an equivalent polkit rule. Do not grant passwordless sudo
for arbitrary commands. Test with
`sudo -n /usr/local/sbin/prismattyc-la-reclaim-zram --dry-run`.

Pass `--allow-missing-zram` only on hosts that have no zram. The staged
script reclaims swap between stages before the next headroom check.

`gherkin-mutator` acceptance mutation stays for Termwright features
(`scripts/acceptance-mutate.sh`).

## Seam rules

1. **One box step per claimed effect.** A ticket that changes what the
   user sees or does ships with a step in `tests/native/spaces-e2e.sh` (or a
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
6. **Reviewer box run.** The reviewer builds the PR, runs it in the native test container
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

## PR template

`.github/pull_request_template.md` asks for: what changed, how it was
verified (unit, e2e step, box run, screenshots), the no-op test (what
the step would catch if the change were a no-op), and which seams the change touches. If the no-op answer
is nothing, the step is wrong.

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

The owner runs KDE Wayland (KWin). The native test container has two e2e jobs on one
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

A stale `prismattyc-native-tests` image may lack weston, grim, or wtype.
`tests/native/docker/run.sh spaces-e2e-wayland` installs those three with
pacman when any is missing.

`tests/native/docker/run.sh spaces-e2e` stays X11.
`tests/native/docker/run.sh spaces-e2e-wayland` starts weston
`--socket=pt290-wayland` in the same `prismattyc-native-tests` image.
