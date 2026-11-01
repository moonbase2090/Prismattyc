# Agent activity

The experimental feed observes headless seats without creating panes. It has a
separate daemon-owned ring and an off-by-default `--experimental-activity` flag.
See [the design note](../../../../docs/agent-activity-feed.md) for schema and limits.

## Build and drive

Build the daemon, adapter, and standalone viewer from the same checkout:

```sh
cargo build -p prismattyc-mux --bins --locked
cargo build --manifest-path tools/activity-monitor/Cargo.toml --locked
python3 scripts/verify-activity.py \
  --bins target/debug \
  --monitor tools/activity-monitor/target/debug/pmux-activity-monitor \
  --out /tmp/activity-proof-new
```

The driver needs Python 3, native binaries, a PTY or Windows ConPTY, and a new output directory. On Windows, use `python` and the monitor executable with its `.exe` suffix. It owns its
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
endpoint, built adapter, and neutral seat. `{hooks}` and `{project}` name a temporary hook profile and workspace. The
producer must load the supplied profile, exit successfully, and publish a
`hooks` or `notify` event to the feed. An independent counter handler must run
both before and after adapter removal. Require `producer_hooks_preserved` in
the result. Add `--producer-failure-hook` only for runtimes supporting that event. Do not put credentials in
the command file or commit local paths. Record which installed runtime/version
was exercised separately from the portable synthetic hook fixture.

## Viewer UI

For changes to the viewer's layout, keys or rendering, also record the real
viewer. Start `tools/activity-monitor/scripts/demo-feed.py --bins target/debug
--sockets FILE` and let it run for a few minutes so lanes have history. Then
drive the monitor with `tools/activity-monitor/scripts/record-demo.py --plan
PLAN --out CAST -- MONITOR --host mac=... --host-cmd windows '[...]'`, render
the cast with `agg`, and inspect the frames. A plan resizes the pseudo-terminal
the way a pane split does, sends keys, and can signal the demo feed to stop and
restart the last host's daemon. Cover each layout tier the change touches
(at least 150x36, 100x30, 64x20, 34x14 and 22x3), the timeline, and an outage.
When frames look wrong, replay the cast in a VT emulator before blaming the
viewer, because GIF encoders can leave faint artifacts.

`cargo test --manifest-path tools/activity-monitor/Cargo.toml --locked`
renders every tier through Lunatui and checks text, colors and keys. The
recorded cast shows the cell path only. `record-demo.py --kitty` exercises the
pixel path, and `extract-rasters.py` saves the rasters it uploaded. Prismattyc
panes keep cell lanes, because its emulator accepts only PNG uploads.
`record-prismattyc.py` keeps a real Prismattyc host's presented frames and
splits the monitor's pane through the control protocol. It needs an unlocked
macOS desktop with an awake display. A clean Tart guest works, because it logs
in automatically: run it over SSH in the guest.

## Boundaries

The native transport and driver support Unix and Windows. Require an actual
Windows runtime drive to claim Windows proof. Cross-compilation is insufficient.
Runtime lifecycle proof does not establish every tool event or hosted-tool
coverage. The feed must remain off by default.
