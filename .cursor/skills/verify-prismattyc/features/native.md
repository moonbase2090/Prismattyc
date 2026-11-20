# Native terminal sessions and images

With `--experimental-rich`, an eligible direct local PTY can negotiate a native
grid and uploaded images. The feature is off by default. An installed native
scene supplies displayed text, selection, find, links, accessibility, cursor
and painting. The retained ANSI terminal becomes visible when ownership ends.

Linux supports grid minor 0 and image minor 2 on 64-bit, little-endian targets
with atomic 64-bit operations. Native Apple Silicon macOS supports image minor
2. The advertised tokens match these session paths. Windows, Intel macOS,
forwarded sessions and daemon-owned log views use the Classic display source.
The protocol is documented in Lunatui's
[grid contract](https://github.com/moonbase2090/lunatui/blob/main/docs/native-protocol.md)
and [image contract](https://github.com/moonbase2090/lunatui/blob/main/docs/native-image-protocol.md).

## Launch and drive

Use the optimized matching binaries from Launch. The helper builds a separately
locked fixture against pinned Lunatui source. It uses the actual Image widget,
image geometry, ANSI encoder, discovery decoder and grid frame encoder. This
is a test producer; it does not enable a production Lunatui native backend.

```sh
python3 .cursor/skills/verify-prismattyc/scripts/verify-native.py \
  --bins "$verify_bins" --out "$verify_evidence/native" --case all
```

Python 3.11+ and Rust 1.90.0 are required. macOS needs an allocated unlocked
desktop and existing Accessibility permission for the runner. Both `tiles`
and `iosurface` run by default; `--presenter` selects one. Linux needs Xvfb,
xdotool and libX11 plus `scripts/install-render-test-deps.sh` dependencies.
The Linux cleanup path needs a kernel with pidfd support and mounted procfs.
Each drive owns a fresh host, daemon, producer and isolated data. The Linux
drive also owns its X server. The helper preserves failure
artifacts and stops at the first failed case.

Keep the macOS fixture window on the main display. The driver normalizes its
font size for that display's 1× or 2× backing scale, then requires the bundled
font's measured 22×49 physical cell for the fixed pixel vectors. A different
display or font measurement is an unmet fixture prerequisite.

Use `--case images` for upload, READY placement, move, omission, retirement and
release. `--case off` checks actual widget ANSI fallback without the native
gate. On Linux, `--case legacy` negotiates minor 0 through the existing decoder
and frame encoder, and `--case resize` resizes the real X11 window, checks ANSI
recovery and activates a fresh descendant on the retained shell endpoint.
On Linux, `--case text` copies a native scene with a wide glyph, a 21-scalar combining
cluster and a URL through real keyboard shortcuts and the isolated X11
clipboard. It then releases ownership and copies the retained Classic scene.
With `smart_ctrl_c = true` in its private configuration, it also opens Find,
types a native-only query and copies the matched text while Find stays open.
The clipboard reader waits for selection events. It does not read host internals.
The retained ANSI baseline is on the alternate screen with mouse tracking.
Find must stay open after its first query key. A plain drag reaches the
application as actual SGR mouse bytes without copying a host selection, and
Shift-drag selects native text. `--case text-no-mouse` repeats the reader drive
with tracking disabled and requires plain drag to select native text. Both
cases copy retained Classic text with Shift-drag after release.

macOS adds these cases:

- `text` reads the owned application's actual Accessibility tree, including the
  full combining cluster and URL. After release it requires Classic text and
  the absence of the native-only text. It does not access the system clipboard.
- `ctrl-z` sends native Ctrl-Z, observes the stopped foreground process, runs
  the shell's real `fg`, checks ANSI recovery and activates a new descendant.
- `lease` stops only the producer after a completed heartbeat. The shell keeps
  the inherited endpoint. Require one lease revocation before resume, ANSI
  recovery and successful activation by a new descendant.
- `metrics` changes the configured font size while heartbeats continue.
  Require SESSION_ENDED reason Resize, a new terminal grid and physical cell
  metrics, ANSI recovery and fresh activation on the same launch endpoint.
- `budgets` admits two 64 MiB sources, declines another source at the 128 MiB
  live limit, preserves actual widget fallback and rejects an oversized source
  before upload. It compares literal pixels with real iTerm2 PNG output.
- `animation` supplies 150 distinct 64×64 checker images at 30 Hz for five
  seconds in a small window. Every presented generation must match the literal
  crop and nearest-sampled pixels. The checker distinguishes image composition
  from cell fallback. This is workload proof, not a comparative benchmark.

## Control replies during revocation

Run the native socket tests with one test thread:

```sh
cargo test --locked -p prismattyc-host --bin prismattyc-host native:: -- --test-threads=1
```

The real socket-pair cases revoke a session before receiving a queued PING,
after queuing its reply and after writing a one-byte reply prefix. An unwritten
session reply must become SESSION_ENDED and be readable after the next poll
without advancing the test clock. A partial reply must finish as one intact
record. Check the old request identity, end reason, unchanged absolute response
deadline, fresh HELLO and preserved setup descriptor. A revoked pending
ACTIVATE must never install a scene. On Linux, minor 0 finishes any pending
reply before closing its generation.

A received PING must have a readable PONG when that same poll returns, before
the host starts painting. The socket test reads without waiting or another
poll. Pending-reply revocation cases fill the real socket's send buffer to
force backpressure; draining those bytes permits the original response to
finish without changing its deadline. This check does not establish liveness
for a request that arrives while painting is already blocked.

The scene tests publish more than one ring's worth of full snapshots without
installing them. Consumption must continue, obsolete pending snapshots must be
released, and the next installation must contain the newest literal glyph.
Coalesced omission frames still leave retirement BUSY while an older painted
scene retains its pixels.

The service socket tests hold the UI's paint snapshot for more than 200 ms
while a real producer continuously publishes frames and sends PING. Every
PONG must arrive within the unchanged 100 ms bound, ring consumption must
continue, and the next UI installation must show the newest literal glyph.
Another case fills all four slots before allowing the worker to copy, then
requires a timely PONG, consumption through the newest frame and its literal
glyph at the next UI installation. Backlog turns service control between copies.
They also check stale installation acknowledgements, worker-side revocation
before ANSI parsing, invalid request identities, worker panic and a blocked
worker command. A maximum valid 1 MiB slot exercises copying and validation
while a public `pty_bytes` call requests revocation. The call must revoke the
scene or hit the unchanged 100 ms watchdog and close the service for ANSI
recovery. This checks the production RPC, without setting a debug-build
performance gate. Measure maximum-slot cost separately in an optimized build.
These component checks complement the real desktop drives and measurements.
The PTY integration fixture drives actual wake notifications until installation;
a fixed number of manual polls does not await an asynchronous worker.

A HELLO received before offer authorization has session zero. Revocation drops
that obsolete offer without a reply; the client recovers to ANSI at its 200 ms
setup bound, and the endpoint accepts a fresh offer. The socket test checks
both a waiting HELLO and a writer receipt arriving just before resize.

The real PTY native-handoff test also installs primary-screen cell-rect and
viewport Rich overlays before discovery. Native discovery and its writer
receipt reset the old Rich session. Require no Classic overlay during Native
or after release; the retained ANSI cells recover without resurrecting the
old Rich capability grant.

## Inspect the result

Require suite and per-drive `result.json` to report PASS. Inspect the logs,
binary SHA-256 records, presented PNGs and saved render-status records. Native
proof names the owned host, selected `display_source`, installed frame sequence,
metrics, grid and placement count. Static captures must match a successful
present and its capture sequence. macOS checks the requested presenter.
The suite requires host, daemon and producer version receipts to name the
current source revision. A supplied `--probe` must come from a matching helper
build; hashes alone do not establish its source provenance.

Require all recorded processes to be reaped and no remaining owned producer.
Retained-shell cases must keep the same root PID and birth identity, advance
the generation and restart frame numbering at 1. Keep runtime directories
private, retain evidence outside them and scrub public proof before uploading.
Presented-frame artifacts prove the application presentation path; OS window
decoration or compositor claims require an OS screenshot separately.

## Text readers and failure cases

Run the host's native, Content and PTY integration tests for protocol bounds,
stale generations, FD ownership, full graphemes, wide-pair lead styles,
copy/find/link/accessibility source selection and damage transitions:

```sh
cargo +1.90.0 test -p prismattyc-host --locked native -- --test-threads=1
cargo +1.90.0 test -p prismattyc-host --locked content::tests -- --test-threads=1
cargo +1.90.0 test -p prismattyc-host --locked mux::native_tests -- --test-threads=1
cargo +1.90.0 test --locked \
  --manifest-path .cursor/skills/verify-prismattyc/scripts/native-client/Cargo.toml
cargo +1.90.0 fmt --check \
  --manifest-path .cursor/skills/verify-prismattyc/scripts/native-client/Cargo.toml
cargo +1.90.0 clippy --locked --all-targets \
  --manifest-path .cursor/skills/verify-prismattyc/scripts/native-client/Cargo.toml \
  -- -D warnings
python3 .cursor/skills/verify-prismattyc/scripts/check-native-client-controls.py \
  --out "$verify_evidence/native-client-controls"
```

Require nonzero test counts. The control helper must detect every deliberate
break and restore the source bytes. Run it with no concurrent writer or build
of the fixture source. Supporting Rust tests do not replace real window proof.

When changing native text or input, additionally drive real selection/copy,
find and link hover against a native text scene, and inspect accessibility text
and the full combining cluster. After release, repeat one reader against the
retained Classic source. The Linux text fixture above proves selection, copy,
find, application mouse priority and Classic recovery. The macOS text case
checks Accessibility on each presenter. Link hover still needs its own desktop
drive. Native has no scrollback; Classic scrollback remains readable.

## Experimental limits

Negotiating Native ends the pane's Rich attachments. Release restores the
retained ANSI grid without restoring those attachments. Applications that use
both protocols must negotiate Rich again after release.
