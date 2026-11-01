# Activity monitor prototype

This opt-in terminal viewer reads the experimental activity feed from one or more daemons. It shows every seat's state and recent activity, including seats with no terminal pane.

Build the daemon from the repository root and this standalone tool with its own lockfile:

```sh
cargo build -p prismattyc-mux --bin pmuxd --locked
cargo build --manifest-path tools/activity-monitor/Cargo.toml --locked
```

Start a private daemon with `--experimental-activity` and a separate socket. Then pass that socket to the viewer:

```sh
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock
```

The `pmux activity` commands, added separately, include `pmux activity monitor`. It runs this binary from beside `pmux` with pmux's socket and passes other arguments through, so a blank Prismattyc pane needs only that command.

The live view fits the pane and follows it through splits and resizes. It groups seats by host, colors each seat by state (working, waiting, failed, stopped), marks rows stale after 30 seconds or while their host is unreachable, and draws a five-minute lane of activity for each seat. Enter opens a seat's timeline. `?` lists the keys and `q` quits. [The UI design](../../docs/activity-monitor-ui.md) covers the layout tiers, colors, states and keys.

The monitor shows identifiers, event kinds, sources, sequences and timing. It does not display command arguments, tool output, file contents, prompts or message bodies, and the feed does not carry them.

### More than one host

Each host has its own daemon, so each gets its own cursor and connection state. `--socket` is this machine, labelled `mac`, `linux` or `windows`. Add more with `--host LABEL=SOCKET`, or read another machine through a command that prints `tail` JSON lines:

```sh
pmux-activity-monitor --socket /tmp/activity-demo/control.sock \
  --host-cmd windows '["ssh","windows-host","pmux","activity","tail","--from-oldest"]'
```

`--host-cmd` takes a JSON argv and runs it without a shell. Its stderr is discarded so remote diagnostics cannot corrupt the screen. A command that exits is restarted with backoff, and replayed records are applied once.

### Display options

`--theme auto|dark|light` picks the Prismattyc Graphite palette; `auto` follows the background color the terminal reports. `--no-motion` turns off the fade when a seat changes state. `--no-pixels` keeps lanes in block characters. Where the terminal accepts raw RGBA Kitty uploads, lanes render as rasters through Lunatui's pixel sparkline. Prismattyc's emulator currently shows only PNG uploads, so inside Prismattyc lanes stay in cells.

The monitor keeps at most 128 seats per host and 256 events per seat, and evicts the seat with the oldest event when full. A daemon restart or retention gap clears that host's cached seats before the retained history replays.

For a single bounded read, use one of these modes:

```sh
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --snapshot --size 80x24
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --json
```

`--snapshot` waits up to three seconds for every host to catch up, then renders one frame as text at `--size` (120 by 24 by default). It fails when no host is reachable. `--json` prints one page from the daemon, including its cursor and reset reason.

Lunatui's own run loop has a fixed grid size, so the monitor drives the Lunatui session, encoder and pixel layer from its own loop. Network requests and commands run on background threads, and pages wait in a bounded channel with their cursor until the UI takes them. Lunatui handles terminal setup and restoration on Unix and Windows. Windows runtime behavior of the live view has not been verified.

The Lunatui crates use the v0.1.18 release commit `d847e749eb773f7d48edd7d9e429bcc9cbe9e898`.

### Recording the UI

`scripts/demo-feed.py` starts private daemons for `mac` and `windows` and publishes scripted metadata events. `SIGUSR1` stops the last host's daemon and `SIGUSR2` starts it again. `scripts/record-demo.py` runs the unmodified monitor in a pseudo-terminal, sends keys and pane resizes from a JSON plan, and writes an asciicast of exactly what the monitor printed. Render it with `agg`. `--kitty` answers the capability probe as a Kitty-graphics terminal so the pixel path runs, and `scripts/extract-rasters.py` saves each raster the monitor uploaded. `scripts/record-prismattyc.py` runs the monitor in a real Prismattyc pane and keeps the host's presented frames; it needs an unlocked macOS desktop.

## JSON connector modes

Shell clients can query the same local feed without a terminal:

```sh
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --seats
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --recent worker-a
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --tail
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --tail --seat worker-a
```

`--seats` returns a JSON object with `epoch`, `cursor`, `oldest_sequence`, `observed_through_sequence`, and `seats`. Each seat entry has its identifier, first retained sequence, last sequence, last receive timestamp, last source, last kind, and retained event count. These are seats observed in retained history. Presence in this list does not establish that a process is alive, and an absent seat may have aged out of retention.

`--recent SEAT` returns the same history metadata, the requested `seat`, and its `records` in sequence order. An unobserved seat returns an empty list. Both finite modes include `schema_version: 1` and `retained_history_only: true`. They freeze the initial feed watermark, read at most two pages covering the 512-event ring, and exclude events newer than that watermark. A restart or retention gap during the read fails the query with a retry message. A busy producer cannot make a finite query follow new pages indefinitely.

`--tail` emits one compact JSON object per line and flushes each line. Activity lines have `type: "activity"`, `seat_filter`, and `page`. The page keeps the daemon's full cursor, reset reason, and pagination metadata even when the optional seat filter removes every record. Consumers advance with `page.cursor`, including after an empty filtered page, and clear their cache when `page.reset` is present. A changed epoch identifies a different daemon lifetime.

Tail reads at most one page every 250 milliseconds. It suppresses unchanged idle pages. A connection failure emits `type: "disconnected"`, `retrying: true`, and the last cursor once per outage. The process retries and emits a new activity page on recovery. It does not configure a terminal. Interrupt or terminate the process to stop it. A closed output pipe ends the process successfully on the next write.
