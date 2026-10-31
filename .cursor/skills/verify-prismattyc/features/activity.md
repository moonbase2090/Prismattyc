# Agent activity

The experimental feed observes headless seats without creating panes. It has a
separate daemon-owned ring and an off-by-default `--experimental-activity` flag.
See [the design note](../../../../docs/agent-activity-feed.md) for schema and limits.

## Build and drive

Build the daemon, adapter, and standalone viewer from the same checkout:

```sh
cargo build -p prismattyc-mux --bins --example activity-emit --locked
cargo build --manifest-path tools/activity-monitor/Cargo.toml --locked
python3 scripts/verify-activity.py \
  --bins target/debug \
  --monitor tools/activity-monitor/target/debug/pmux-activity-monitor \
  --out /tmp/activity-proof-new
```

The driver needs Unix, Python 3, a PTY, and a new output directory. It owns its
daemon and viewer and uses isolated HOME, XDG paths, configuration, and socket.
It checks the owned daemon process and pings its private socket before driving.
Never use the operator's daemon for this proof.

Require `status: PASS`, `direct_processes_reaped: true`, `viewer_exit_observed: true`, and
`scratch_removed: true` in `result.json`. Inspect `viewer.txt` for `worker-live`
and `command_started`, and retain `live-terminal.txt`. The live assertion starts
the viewer first, then emits a hook event and observes its seat without keyboard
input. A snapshot alone does not establish live updates.

The same drive verifies rejection with the flag absent, omitted command content,
duplicate/conflicting retries, page cursors, retention reset, boot epoch reset,
unchanged pane/process identity, and the JSON connector modes. `--seats`,
`--recent`, and a filtered `--tail` run against the real daemon. If it fails, retain its result and daemon log, check the
failure, and use a fresh output directory when rerunning.

For actual harness proof, add `--producer-command FILE`, a reviewed JSON argv
array. `{socket}`, `{emitter}`, and `{seat}` expand to the driver's private
endpoint, built adapter, and neutral seat. The producer must exit successfully
and publish a `hooks` or `notify` event to the feed. Do not put credentials in
the command file or commit local paths. Record which installed runtime/version
was exercised separately from the portable synthetic hook fixture.

## Boundaries

The native transport has Unix and Windows implementations. The PTY driver only
verifies Unix. Windows needs a separate runtime drive, and clean-VM proof remains
subject to repository policy. No live verification is implied for a runtime
whose upstream hook interface is unconfirmed.
