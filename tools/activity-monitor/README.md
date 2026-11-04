# Activity monitor

The monitor shows seat state and recent metadata from one or more daemons,
including seats with no terminal pane. Its UI runs as a contained plug-in.
The native launcher presents validated frames; JSON modes use the contained
activity connector. A release enables both packages with local activity
access the first time its daemon starts, and keeps them off once disabled.

### Use it from a release

A matched release carries `pmux-activity-monitor` and verified packages for
`org.moonbase.activity-monitor` and `org.moonbase.activity-connector`. There
is nothing to set up: the first time a release daemon finds either package
with no saved state, it installs the bundled version and enables it with
`activity.read:local` only. Remote endpoints still need an explicit grant.
Run `pmux-activity-monitor` in a pane.

To turn either one off, disable it in Settings > Plug-ins or run:

```sh
pmux plugin disable org.moonbase.activity-monitor
pmux plugin disable org.moonbase.activity-connector
```

A disabled package stays disabled across restarts and updates; the daemon
only fills in plug-ins that have no saved state. To turn one back on:

```sh
pmux plugin enable org.moonbase.activity-monitor --allow activity.read:local
```

Use the same `--socket` target on these commands if the launcher uses an
explicit socket. A package you install by hand stays disabled until you
enable it. The monitor package serves live and snapshot modes. The connector serves `--json`, `--seats`,
`--recent`, and `--tail`. Review the available version before selecting a
later package; updates and grants remain explicit.

Collection is separate and off by default. Turn it on in Settings under Mux,
or in the config file:

```toml
[mux]
experimental_activity = true
```

The running daemon applies collection changes within a second without a
restart. Sessions stay alive. After approval and collection enablement,
`pmux-activity-monitor` is the whole command in a pane. Setting collection
back to `false` stops the feed and discards its history.

After upgrading, a daemon with no saved plug-in state enables both packages
as above; one you disabled stays off. On Linux and Windows, an
older updater still manages six core executables. Update the core first,
then run the new command `pmux update --repair-monitor-launcher`; use
`--from-monitor-version 0.3.54` for an unreceipted official 0.3.54 launcher.
See [launcher repair](../../docs/update-and-restart.md#replace-an-older-activity-monitor-launcher)
for ownership checks, dry checks, and rollback. macOS replaces the verified app.

### Build from source

Prepare a verified bundle before building any consumer. The default uses the
reviewed official pin. For private development, explicitly set
`PRISMATTYC_PLUGIN_BUNDLE_DIR` to an existing verified candidate first. A source
build without a bundle has an empty catalog and cannot install these packages.

```sh
export CARGO_TARGET_DIR="$PWD/target"
PRISMATTYC_PLUGIN_BUNDLE_DIR=$(python3 scripts/release/plugin_bundle_provenance.py prepare)
export PRISMATTYC_PLUGIN_BUNDLE_DIR
CARGO_ENCODED_RUSTFLAGS=$(python3 scripts/release/plugin_bundle_provenance.py flags)
export CARGO_ENCODED_RUSTFLAGS
cargo +1.90.0 build --workspace --bins --locked
cargo +1.90.0 build --manifest-path tools/activity-monitor/Cargo.toml --target-dir "$CARGO_TARGET_DIR" --locked
```

Start a private daemon in a separate terminal with a private absolute socket,
then repeat the install/enable commands above with that socket on every command:

```sh
mkdir -m 700 /tmp/activity-demo
target/debug/pmuxd --experimental-activity --socket /tmp/activity-demo/feed.sock -- /bin/sh
```

After approval, open the viewer:

```sh
target/debug/pmux-activity-monitor --socket /tmp/activity-demo/feed.sock
```

With no `--socket` or `--host`, the monitor selects the daemon the same way
`pmux` does. The live view follows pane splits and resizes. Enter opens a
seat's timeline, `?` lists keys, and `q` quits. It shows identifiers, event
kinds, sources, sequences, and timing. The feed carries no command arguments,
tool output, file contents, prompts, or message bodies.
[The UI design](../../docs/activity-monitor-ui.md) covers layout and keys.

### More than one host

Each host has its own daemon, so each gets its own cursor and connection state. `--socket` is this machine, labelled `mac`, `linux` or `windows`. Register each remote transport and approve its named grant first. For example:

```sh
pmux plugin endpoint add windows --command-tail '["ssh","windows-host","pmux-activity-monitor","--tail"]'
pmux plugin enable org.moonbase.activity-monitor --version 1.0.0 --allow activity.read:local --allow activity.read:windows
pmux-activity-monitor --host-cmd windows '["ssh","windows-host","pmux-activity-monitor","--tail"]'
```

`--host-cmd` must exactly match its registered transport. Importing or registering an endpoint grants no access. The remote daemon also needs its connector installed and enabled. The native broker runs the approved JSON argv without a shell. Its stderr is discarded so remote diagnostics cannot corrupt the screen. A command that exits is restarted with backoff, and replayed records are applied once.

### Display options

`--theme auto|dark|light` picks the Prismattyc Graphite palette; `auto` follows the background color the terminal reports. `--no-motion` turns off the fade when a seat changes state live (replayed history never fades). `--no-pixels` keeps lanes in block characters. Where the terminal accepts raw RGBA Kitty uploads, lanes render as rasters through Lunatui's pixel sparkline. Inside Prismattyc the monitor first makes one real raw upload: a Prismattyc that keeps it gets pixel lanes, and older releases, which drop raw uploads, keep cells.

The monitor discovers every daemon session with an agent identity on its first
poll and refreshes that list while open. Hooks add lifecycle and tool details;
quiet seats appear without hooks and remain visible after event history expires.
Without hook state, recent pane output means working, 60 seconds of silence
means waiting, and no live pane child means stopped. The viewer removes a
session with no retained events when the daemon no longer reports it.

Each seat keeps at most 256 events. The 128-seat history limit applies to seats
that are absent from the current session list; current sessions are not evicted.
A daemon restart or retention gap clears cached history and reloads the current
session list. Older endpoints that omit session metadata show retained events
only.

For a single bounded read, use one of these modes:

```sh
target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --snapshot --size 80x24
target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --json
```

`--snapshot` waits up to three seconds for every host to catch up, then renders one frame as text at `--size` (120 by 24 by default). It fails when no host is reachable. `--json` prints one page from the daemon, including its cursor and reset reason.

The native presenter owns the Lunatui session, encoder, pixel layer, and terminal restoration. The guest owns its model and layout. The native broker owns remote requests and commands. Native Windows runtime proof remains required before claiming that platform is verified.

The Lunatui crates use the v0.1.18 release commit `d847e749eb773f7d48edd7d9e429bcc9cbe9e898`.

### Recording the UI

The recording helpers create private daemons but do not install packages or approve grants. Provision the monitor, connector, and requested endpoints in those private stores before recording.

`scripts/demo-feed.py` starts private daemons for `mac` and `windows` and publishes scripted metadata events. `SIGUSR1` stops the last host's daemon and `SIGUSR2` starts it again. `scripts/record-demo.py` runs the unmodified monitor in a pseudo-terminal, sends keys and pane resizes from a JSON plan, and writes an asciicast of exactly what the monitor printed. Render it with `agg`. `--kitty` answers the capability probe as a Kitty-graphics terminal so the pixel path runs, and `scripts/extract-rasters.py` saves each raster the monitor uploaded. `scripts/record-prismattyc.py` runs the monitor in a real Prismattyc pane, splits that pane through the control protocol, and keeps the host's presented frames. It needs an unlocked macOS desktop with an awake display, such as a clean Tart VM.

## JSON connector modes

Shell clients can query the same local feed without a terminal:

```sh
target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --seats
target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --recent worker-a
target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --tail
target/debug/pmux-activity-monitor --socket /tmp/activity-demo/control.sock --tail --seat worker-a
```

`--seats` returns a JSON object with `epoch`, `cursor`, `oldest_sequence`, `observed_through_sequence`, and `seats`. Each seat entry has its identifier, first retained sequence, last sequence, last receive timestamp, last source, last kind, and retained event count. These are seats observed in retained history. Presence in this list does not establish that a process is alive, and an absent seat may have aged out of retention.

`--recent SEAT` returns the same history metadata, the requested `seat`, and its `records` in sequence order. An unobserved seat returns an empty list. Both finite modes include `schema_version: 1` and `retained_history_only: true`. They freeze the initial feed watermark, read at most two pages covering the 512-event ring, and exclude events newer than that watermark. A restart or retention gap during the read fails the query with a retry message. A busy producer cannot make a finite query follow new pages indefinitely.

`--tail` emits one compact JSON object per line and flushes each line. Activity lines have `type: "activity"`, `seat_filter`, and `page`. The page keeps the daemon's full cursor, reset reason, and pagination metadata even when the optional seat filter removes every record. Consumers advance with `page.cursor`, including after an empty filtered page, and clear their cache when `page.reset` is present. A changed epoch identifies a different daemon lifetime.

Tail reads at most one page every 250 milliseconds. It suppresses unchanged idle pages. A connection failure emits `type: "disconnected"`, `retrying: true`, and the last cursor once per outage. The process retries and emits a new activity page on recovery. It does not configure a terminal. Interrupt or terminate the process to stop it. A closed output pipe ends the process successfully on the next write.
