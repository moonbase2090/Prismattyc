# Prismattyc demo reel recorder (Linux / Docker)

Automated screen recording of `prismattyc-host` for the website reel.
`record-demo.sh` drives the windowed host through a feature tour with
`xdotool`, records the Xvfb display with `ffmpeg x11grab`, and lays one
ElevenLabs narration clip per beat at the moment that beat started. Beats
that involve live agents take as long as the agents take; the narration
follows the recorded timestamps, so the voice never runs ahead of the screen.

Output: `~/Desktop/demo-reel.mp4` (1920×1080, H.264 + AAC). The website
expects it at `prismattyc-website/assets/demo-reel.mp4`.

## What the reel shows

Splash · hotkey footer · command palette · theme picker · truecolor,
unicode, cell-grid sprites (box/arcs/braille/powerline) · kitty graphics ·
scrollback + find · splits, quadrants, zoom · tabs, rename, pane handles ·
`pmux ls`, attach/detach · spaces (save, open from the rail) · two agent
seats — Claude Code and Kiro — exchanging a letter over `pmux-mcp`, with
the doorbell (`PMUX_MAIL` injection + tab envelope) on camera · intentional
pane messaging: execute `seq 1 10000`, inspect output, then remove and kill
the test session.

## Files

- `record-demo.sh` — recording driver. Use `--check` to check dependencies,
  `--launch` to open the host, `--dry` for a silent take, or `--clips` to
  generate narration for a voice audition. Run without an option for a narrated take.
- `parts/` — test cards: `ansi.sh`, `colors.sh`, `unicode.sh`, `box.sh`,
  `image.sh` (+ `prismattyc-256.png` for the kitty beat).
- `docker/` — the generic `demo@prismattyc` box: Arch + Xvfb + Openbox,
  the host bins, `pmux-mcp`, `claude`, `kiro-cli`, the host `config.toml`,
  and the Kiro agent config (`kiro-agent.json` → `~/.kiro/agents/pmux.json`).
- `.eleven.env.example` — template for the ElevenLabs key (copy to `.eleven.env`).

## Setup

Host needs Docker, the Prismattyc bins on `PATH`, `claude` and `kiro-cli`
on `PATH` and logged in, and (for a narrated take) `demo/.eleven.env`.

```bash
cd demo/docker
./run.sh --check     # builds the image, checks tools inside the box
./run.sh --dry       # silent take to tune pacing (no ElevenLabs)
./run.sh --clips     # generate or reuse the voice clips without recording
./run.sh             # narrated take -> ~/Desktop/demo-reel.mp4
./run.sh host-ux-e2e # PT-303: caret, light-cycle, and cold-start restore
./run.sh spaces-e2e  # PT-217: switch/--add/save/+ /chip in the box
./run.sh walkthrough-caption-e2e  # PT-295/297: double-click [show me], caption +1
./run.sh render-bench  # PT-246: render_timer=log every-frame experiments vs foot
```

`spaces-e2e` uses `docker create --init` and copies `pmux` / `pmuxd` /
`pmux-attach` / `prismattyc-host` from `PRISMATTYC_BINS`, else
`<repo>/target/debug`, else PATH (with a warning). Runs
`demo/spaces-e2e.sh` inside the box. Build the image first.
`WIGGLE=1` enables an optional pointer nudge after space opens; it is off by default so idle-host checks remain honest.

`walkthrough-caption-e2e` copies the same bins and runs
`demo/walkthrough-caption-e2e.sh`. It double-clicks `[show me]` and
asserts the caption advances one step and does not skip. No wiggle.

`render-bench` copies the same bins, runs `demo/render-bench.sh`, and
writes `build/render-bench/<version>/{summary.tsv,metrics.json}` on the
host. Foot and `notcurses-demo` install via pacman when the box can
reach Arch mirrors; the prismattyc log parse still runs if they are
missing.

The recording commands copy agent credentials into `~/.cache/prismattyc-demo/creds`.
They mount the copies from that directory. The box does not use the operator's live files.
Claude refreshes stay in the private staged directory between takes. The runner
uses a newer host credential after you sign in again. It does not overwrite a
refreshed demo credential with an older copy.

## Re-record the narrated reel

1. Use the installed binaries for the version you want to show.
2. Set a separate output directory for the silent take.
3. Run the silent take and inspect the feature scenes and both agent replies.
4. Run the narrated take into another output directory.
5. Check the picture, narration timing, and ending before replacing `demo-reel.mp4`.

```bash
PRISMATTYC_BINS="$HOME/.cargo/bin" \
PRISMATTYC_DEMO_OUTDIR="$PWD/build/reel-dry" demo/docker/run.sh --dry

PRISMATTYC_BINS="$HOME/.cargo/bin" \
PRISMATTYC_DEMO_OUTDIR="$PWD/build/reel-voiced" demo/docker/run.sh
```

The script records each scene's start time in `demo-beats.tsv`. Agent scenes
wait for real mailbox activity. They are not simulated. Narration is mixed
at those timestamps. The final mix replaces the output file only after its
length is checked. The script retains the raw capture if mixing fails.

## Run host UX regression checks

1. Build the binaries for the checkout under test.

   ```bash
   cargo build --locked -p prismattyc-host -p prismattyc-mux --bins
   ```

2. Run the fixture when no other Local Actions job uses Docker.

   ```bash
   PRISMATTYC_BINS="$PWD/target/debug" demo/docker/run.sh host-ux-e2e
   ```

The fixture uses a fresh Xvfb container. It copies the four tested binaries.
It does not stage agent credentials. A missing binary, image, native window,
pixel capture, or completion marker fails the run. It does not use a binary
from `PATH` as a fallback.

| Check | Required result |
| --- | --- |
| Left and Home | Bash readline receives the edits. The visible caret moves before another text edit or bell. Both keys use partial raster. |
| Light-cycle | At least three distinct intermediate trails form a continuous clockwise sweep. Only one vehicle head remains during the sweep. No head pixels remain after it settles. |
| Cold start | The host asks before restoration. Decline keeps a fresh window and the saved cache. The next cold launch asks again. Accept restores two tabs. Enter revives the focused session, whose guest output must appear in both frame and X11 captures. |
| Restart recovery | Create without a daemon. Correct a duplicate session name in the same dialog. Reopen after window and daemon restarts. Reuse an old numeric ID for an unrelated session and prove its PTY receives no input. Recover an older cache. Create after a deleted-Space restore fails. Require distinct output colors in the presented frame and X11 capture, including output at the right edge. |
| Space open race | Click A, B, then C while a wrapper delays the real `pmux` for five seconds. Helpers run in order. After idle, the current chip, cache, selected tab, and focused session agree with C. Version 1 Space files remain unchanged. |
| Open outcomes | A saved two-pane session retains three live panes and reports reuse. The receipt remains after the toast expires. Injected helper failure and missing-cache success keep the previous Space. A session that stops after apply is reported unavailable. |

The script derives pane bounds and caret cell dimensions from the rendered
pixels. It checks CPU frame dumps and captures the actual X11 display.

Evidence remains in `build/host-ux-e2e/<run-id>/` on success and failure.
Restart evidence is in the `restart-spaces/` subdirectory. To run only
those checks on Linux with Xvfb, xdotool, and FFmpeg installed:

```bash
PATH="$PWD/target/debug:$PATH" python3 demo/restart-spaces-e2e.py
```

This command creates its own display, daemon, home, and XDG directories.
It writes to `build/restart-spaces/`. Set `RESTART_SPACES_OUT` to a new
directory for another run.

`result.json` records the verdict and host binary SHA-256. The directory also
contains host logs, frame metadata, PNGs, and border measurements. Set
`HOST_UX_RUN_ID` to choose a run directory name. Use a new name for each run.
Set `HOST_UX_CASE=space-open-race` to run only the race step. The default
runs all checks. `HOST_UX_NO_WM=1` permits the focused step on a private
Xvfb display without a window manager. This mode proves host state and
X11 pixels. The Docker run still proves the window-manager path.

The race step stores its result, helper order, and screenshots in
`space-open-race/`. Its private mux socket and data paths isolate it from
the earlier steps. Replacing the open queue with concurrent helpers must
fail its order assertion.

The Local Actions `spaces-e2e` job runs both the existing spaces fixture and
this fixture. Its artifact mount preserves evidence in the host checkout
after container cleanup.

To check the animation failure path, set `HOST_UX_ANIMATION=none`. The same
assertions must fail because the border has no intermediate sweep.

## Notes

- `.eleven.env` holds a secret and is git-ignored. Never commit it.
- The reel uses Daniel (`pH8TIDBxKcsLhKFzhwgP`) by default. Set
  `ELEVEN_VOICE_ID` explicitly to select another voice. A saved voice in
  `.eleven.env` does not override this choice. Set `ELEVEN_SPEED` to adjust the pace.
- The recording box is limited to two CPUs and 8 GiB of memory. It uses a private
  Xvfb display and does not need GPU access.
- Set `PRISMATTYC_DEMO_OUTDIR` to a new directory for each take. This directory
  receives the video, beat timestamps, and logs. A failed take also saves a screenshot.
- The entrypoint copies the recording driver before it starts. Later edits cannot
  change an in-progress take. Narration clips regenerate when their text changes.
- Sessions inside the box: `claude` and `kiro` (agent seats) and `work`.
- The Docker recorder drives its private Xvfb display. You can continue to use
  your desktop during the recording.

## Test Space teams in the box

Build the workspace binaries. Put Termwright on `PATH`. Set
`PRISMATTYC_BINS` to the directory that contains those binaries.

```bash
PRISMATTYC_BINS="$PWD/target/debug" demo/docker/spaces-team.sh
SPACES_TEAM_CASE=restart demo/docker/spaces-team.sh
SPACES_TEAM_CASE=race demo/docker/spaces-team.sh
```

The driver copies only test binaries and fixtures into `prismattyc-demo`.
It uses private data, sockets, and Xvfb displays. It acquires the heavy-job
lock and removes its container after copying the evidence. It does not
restart the operator's daemon or copy agent credentials.

The default case wraps the team CLI and native checks in Termwright. It
also captures a real `pmux space attach` session through Termwright.
The native checks drive the desktop with X11 input. The output includes
binary hashes, command results, presented PNGs, display captures, and a
JSON verdict under `build/spaces-team-box/<run-id>/`.

Use `SPACES_TEAM_CASE=move` to test moves from nested terminals. The
fixture opens a parent shell, a sibling, and a nested session. It checks
pane and process IDs after each move. It also checks cancellation when
the selected target exits or changes, live nested session switching,
and ordinary managed pane moves.


## Watch intentional pane messaging

Build current binaries, then record the focused demo:

```bash
python3 demo/pane-messaging-demo.py --bins "$PWD/target/debug" --out "$PWD/build/pane-messaging-demo"
python3 -m http.server 8767 --bind 127.0.0.1 --directory "$PWD/build/pane-messaging-demo"
```

Open `http://127.0.0.1:8767/`. The page contains a silent video and three
captioned screenshots. The sender and receiver share a Space. The sender
runs `pmux pane-write` with `seq 1 10000`, verifies output, and removes and
kills the receiver. The sender remains alive.

The fixture requires Linux, Xvfb, xdotool, and FFmpeg. It uses a private
home, daemon, and display. It does not contact AWS or an AI provider.
`result.json` records commands and receipts. A missing output marker or
remaining receiver pane fails the demo. Use a new output directory each run.

The full reel includes the same CLI workflow through
`parts/pane-messaging.py`. Run a new silent take to include the new segment;
previously rendered MP4 files do not update when the script changes.

### Check native macOS restart rendering

Run this check on a Mac with a logged-in desktop session. Build the host,
mux, and MCP binaries first. The check uses private sessions and configuration.
It does not replace your installed app or restart your existing daemon.

```bash
cargo build --release --locked -p prismattyc-host -p prismattyc-mux -p pmux-mcp
python3 demo/macos-restart-e2e.py --bins "$PWD/target/release" --out "$PWD/build/macos-restart"
```

The output directory must not exist. The check verifies colored terminal
pixels before and after host restart, including idle repaints. It also
checks session process identity, Space switching, daemon restart deferral,
and MCP restart. Inspect the PNGs and `result.json` in the output directory.
These are host framebuffer captures. Keyboard input automation and desktop
screen recording require separate macOS permissions and checks.

The walkthrough voice generator requires Python 3.11 or newer, or Python
with `tomli`. It selects an available compatible interpreter. It does not
install packages into Apple's system Python.
