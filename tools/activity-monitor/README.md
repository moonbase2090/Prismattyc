# Activity monitor prototype

This opt-in terminal viewer reads the experimental activity feed from one local daemon. It shows the latest event per seat, including seats with no terminal pane.

Build the daemon from the repository root and this standalone tool with its own lockfile:

```sh
cargo build -p prismattyc-mux --bin pmuxd --locked
cargo build --manifest-path tools/activity-monitor/Cargo.toml --locked
```

Start a private daemon with `--experimental-activity` and a separate socket. Then pass that socket to the viewer:

```sh
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock
```

Press `q` to exit. The monitor polls every 250 milliseconds, reconnects after errors, and labels cached activity as stale after 30 seconds. A daemon restart or retention gap clears the cached seats before the retained history replays. The daemon's sequence orders events. Receive timestamps determine displayed age.

The live view uses a fixed 120-column by 24-row terminal grid. It keeps at most 128 seats, evicts the seat with the oldest event when full, and reports seats outside the visible rows. It displays identifiers, event kinds, and counters. It does not display command arguments, tool output, file contents, prompts, or message bodies.

For a single bounded read, use one of these modes:

```sh
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --snapshot
tools/activity-monitor/target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --json
```

`--snapshot` renders a 120-column by 24-row text frame through Lunatui. `--json` prints one page from the daemon, including its cursor and reset reason. Both read at most 256 events and exit. If retained history exceeds one page, the snapshot says that catch-up is pending. Use live mode to drain subsequent pages.

The viewer has a bounded background channel. Network requests never block rendering, and queued pages retain their cursor until the UI can receive them. Live mode requests timer frames so new events appear without keyboard input. Lunatui handles terminal setup and restoration on Unix and Windows. Windows runtime verification remains separate from the source-level backend support.

The five direct Lunatui crates use the v0.1.18 release commit `d847e749eb773f7d48edd7d9e429bcc9cbe9e898`. The default image and pixel widget features are disabled.

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
