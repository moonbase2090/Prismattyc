# Activity monitor UI

![The monitor reading two private daemons: overview, timeline, filter, an outage on windows, live resizes through every tier, keys, and the light theme](media/activity-monitor/activity-monitor.gif)

The activity monitor is a terminal app for a person watching agent seats on more than one machine. Its job is to answer three questions in a glance from a narrow side pane: who needs me, who is stuck or failing, and what has this seat been doing. It reads the experimental activity feed in [agent-activity-feed.md](agent-activity-feed.md) and shows metadata only: host, seat, event kind, source, run and event identifiers, sequence numbers and timing. It never shows command text, paths, prompts, tool output or mail bodies, and it has no way to obtain them, because the feed does not carry them.

## Direction

The monitor speaks the same status language as Prismattyc's own chrome. In the Graphite pane chips, green means working, amber means waiting on you and coral means attention. The monitor uses those three colors for the same meanings, so a seat looks the same here as its pane does in the sidebar. Everything else stays quiet: the default terminal background (so pane transparency still shows through), one hairline color, muted secondary text and no boxes around rows.

The monitor puts its visual weight in one place: a lane for each seat. A lane is a strip of time bins that runs from the left edge of its column to now, with each bin colored by the seat's state and as tall as the activity in that bin. A run of tool calls rises as a ridge. A seat waiting on you leaves a flat amber line, and a stopped seat leaves nothing. Lanes give the overview, and the timeline gives the detail.

| Role | Dark | Light | Source |
| --- | --- | --- | --- |
| Working | `#4cc98a` | `#1f8a55` | Graphite working dot |
| Waiting | `#f2b84b` | `#b87a0a` | Graphite unseen/needs-you |
| Failed | `#ff7a6b` | `#c2392b` | Graphite attention |
| Stopped | `#7d8594` | `#8a929e` | Between muted text and hairline |
| Text / muted | `#e6e9ee` / `#a3abb8` | `#1f2329` / `#5b6472` | Graphite text |
| Selection / focus | `#1c2330` / `#5aa2ff` | `#eaf1fc` / `#2f6fd0` | Focused title row / focus color |
| Hairline | `#262a32` | `#d5d9df` | Graphite hairline |

Shape carries state as well as color, so the states stay distinct without color: `●` working, `◆` waiting, `✕` failed, `■` stopped and `·` no state yet. The theme follows the terminal background reported by the probe and falls back to dark. `--theme light|dark` overrides it.

## State

Each event kind sets a seat state. Mail events leave the state unchanged.

| State | Kinds |
| --- | --- |
| Working | `working`, `turn_started`, any `*_started`, `*_finished`, `test_passed` |
| Waiting | `waiting`, `turn_finished`, `session_started` (a new session waits for its first prompt) |
| Failed | `tool_failed`, `command_failed`, `edit_failed`, `test_failed` |
| Stopped | `stopped`, `session_finished` |

Failed stays until the seat reports something else. `*_finished` means an operation ended, so the timeline never shows it as a success. Only `test_passed` gets a check mark.

A row is stale when its last event is 30 seconds old, or when its host is disconnected. A stale row shows its age in amber, and in wider layouts it also says `stale`. Staleness never changes a seat's state, because a quiet seat might be thinking, not hung.

## Hosts

Each host has its own pmuxd, sequence and epoch, so the monitor keeps one cursor, connection state and seat table for each host. It never merges sequences across hosts.

```sh
pmux-activity-monitor --socket /run/pmux.sock                          # this machine, labelled mac/linux/windows
pmux-activity-monitor --host mac=/run/a.sock --host windows=/run/b.sock
pmux-activity-monitor --socket /run/pmux.sock \
  --host-cmd windows '["ssh","windows-host","pmux","activity","tail","--from-oldest"]'
```

`--host-cmd` runs a vetted JSON argv and reads `tail` JSON lines from its stdout. The argument is a JSON array, not a shell string, so nothing gets split or expanded. This reaches a remote machine through the user's own SSH, with no new listener. The monitor drops records it has already applied (same epoch, sequence not newer), so a restarted command replays safely. A command that exits is restarted with backoff. Host clocks can differ: ages from a remote host use that host's receive time, and a negative age shows as 0.

## Layout

The layout is recomputed from the pane size on every frame, so splitting or resizing the pane changes the tier immediately.

```
Wide (>= 110 cols, >= 14 rows): list and timeline side by side
 Activity monitor   ● 5 working  ◆ 2 waiting  ✕ 1 failed  ■ 2 stopped
 mac  connected  7 seats  ▁▂▅▇▅▂▁▂▃  │ worker-a  mac  ● working for 41s
▌● worker-a      command_started   4s ▁▂▅▇▆▃│ ▁▁▂▅▇▆▃▂▅▇▇▆▅▃▁▂▃▅▆▇▅▃
 ◆ reviewer-2    turn_finished    41s ▅▃▁▁▁▁│   4s  +0.4s ▸ command_started hooks  run 3fa2c1e0
 ✕ builder       test_failed       2m ▇▇▅   │  12s  +8.1s · edit_finished   hooks  run 3fa2c1e0
 windows  retrying for 12s  3 seats cached   │ ── run 77ab01c2
 ● win-agent-1   tool_started     19s stale │ ...
 ↑↓ move  ⏎ timeline  ⇥ host  f filter  s sort  p pixels  ? keys  q quit

Medium (72 to 109 cols): list with kind, age and lane; Enter opens the timeline full screen
Narrow (40 to 71 cols): glyph, seat, kind and age; no lane
Small (< 40 cols): glyph, seat and age; header shows counts only (●5 ◆2 ✕1 ■2)
Tiny (< 24 cols or < 4 rows): one line of counts
```

Text is left-aligned, ages are right-aligned in a fixed column, and lanes grow toward the right edge so "now" is always at the same place. Host rows have no box: a host label, its connection state and, in wide layouts, a lane of all its seats' activity.

## Timeline

Enter opens the timeline for the selected seat, and in the wide tier the timeline panel follows the selection. It shows the seat's retained events newest first, up to 256 per seat: age, gap since the previous event, a kind glyph (`○` session started, `▸` started, `·` finished, `✕` failed, `✓` test passed, `◆` waiting, `■` stopped, `»`/`«` mail), the kind, source, short run and event identifiers, and the daemon sequence. A hairline marks each change of run. Above the list, a ten-minute lane and a count summary (events, runs, failures) give the shape of the seat's recent work. Columns drop from the right as the width shrinks: sequence, event identifier, run, source and then gap.

## Keys

| Key | Action |
| --- | --- |
| `↑` `↓` / `k` `j` | Move the selection; in the timeline, scroll |
| `PgUp` `PgDn`, `Home` `End` / `g` `G` | Move by page, or to the first or last row |
| `Tab` / `Shift-Tab` | Jump to the next or previous host |
| `Enter` / `→` / `l` | Open the timeline |
| `Esc` / `←` / `h` | Back to the list |
| `f` | Filter: all, needs attention (waiting, failed, stale), working |
| `s` | Sort: by name, by most recent, by state |
| `p` | Toggle pixel lanes when the terminal supports them |
| `?` | Show the keys |
| `q` / `Ctrl-C` | Quit |

Sorting by name is the default, so rows don't jump while you read them.

## Motion and pixels

When a seat changes state in a live update, its row fades from a tint of the new state color to the normal background over 900 milliseconds. History replayed on connect, after a feed reset or while catching up does not fade, so the first frame is not covered in tints. This is the only animation the monitor starts on its own, and `--no-motion` turns it off.

On a terminal that accepts raw RGBA Kitty graphics uploads, lanes render through Lunatui's pixel sparkline as one raster per lane, with eight times the vertical resolution and no gaps between columns. The cell rendering is drawn first and replaced only when the raster is accepted, so a terminal without pixel support, a multiplexer or `--no-pixels` gets the same lanes in block characters. `p` switches between the two at runtime.

Prismattyc decodes Kitty graphics but accepts only PNG uploads (`f=100`), and Lunatui 0.1.18 uploads raw RGBA (`f=32`). Inside Prismattyc the cleared cells would never be replaced, so the monitor keeps cell lanes when the terminal identifies as Prismattyc. Pixel lanes in Prismattyc need either PNG uploads from Lunatui or RGBA support in the emulator.

## Launch

The `pmux activity` commands, added separately, include `pmux activity monitor [ARGS]`. It runs `pmux-activity-monitor` from beside `pmux` (or from `PATH`), passes pmux's resolved socket as `--socket`, and passes every other argument through. In a new Prismattyc pane, that command is all it takes. A pane preset would need a host configuration change for one command, so the subcommand is the simpler choice. `--snapshot --size COLSxROWS` renders one frame as text for scripts and tests.

## Runtime

Lunatui 0.1.18's `run` uses a fixed grid size, so the monitor drives the Lunatui session, encoder and pixel layer from its own small loop. That loop reads the pane size on every frame, clears and repaints in full after a resize, waits for input with a short timeout, and draws at 30 frames per second only while a fade is running. Otherwise it draws only when something changed or once per second for ages. It uses `TIOCGWINSZ` on Unix and the console buffer size on Windows. Windows runtime behavior has not been verified yet. On quit, the monitor stops each host command with its process group, then gives the feed readers one shared one-second deadline to finish before it exits without them.

## Recordings

[activity-monitor.gif](media/activity-monitor/activity-monitor.gif) is real monitor output. Two private daemons labelled `mac` and `windows` ran on one Mac with the scripted demo feed, and `windows` was read through `--host-cmd`. The monitor ran unmodified in a pseudo-terminal, `record-demo.py` sent the keys and resizes, and `agg` rendered the cast. The scenes are the wide overview, moving the selection, the full timeline, the needs-attention filter, Tab to `windows`, a stopped and restarted `windows` daemon, resizes to 100x30, 64x20, 34x14 and 22x3, the keys panel, and the light theme. The shrinking scenes keep the 150x36 canvas, so a small pane appears in the top-left corner.

[prismattyc-window.gif](media/activity-monitor/prismattyc-window.gif) shows the monitor in a real Prismattyc pane, taken from the host's own presented frames (`PRISMATTYC_DUMP_PRESENT`, timed as presented) in a clean macOS 26.6.2 VM. `record-prismattyc.py` drove it: a drill-in, then a split of the monitor's pane through the control protocol, where the monitor drops to the narrow tier beside the new shell, then the split moving back out. Lanes stay in cells there, as described under Motion and pixels.

[pixel-lane-rasters.png](media/activity-monitor/pixel-lane-rasters.png) stacks nine lane rasters exactly as the monitor uploaded them when the probe reported Kitty graphics (`record-demo.py --kitty`, then `extract-rasters.py`), padded and scaled 2x. It shows the pixel path's output, not a terminal displaying it.
