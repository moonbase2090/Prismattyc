# Render-thread spike harness (not for merge)

Reference code for `docs/design/render-thread.md`. This branch is never
merged. Every switch below is off unless its environment variable is set.

| Variable | Effect |
| --- | --- |
| `PRISMATTYC_SPIKE_TIMING=PATH` | Per-phase timing table rewritten to `PATH` every 2 s; `PATH.reset` clears it |
| `PRISMATTYC_SPIKE_CELLS=COLSxROWS` | First window size in cells |
| `PRISMATTYC_SPIKE_POLL_DRAIN=1` | `Wake` no longer pumps; leftover drain re-arms with `ControlFlow::Poll` |
| `PRISMATTYC_SPIKE_DRAIN_BUDGET_MS=N` | Keep draining PTY output for up to N ms per pump |
| `PRISMATTYC_SPIKE_PRESENT_THREAD=1` | macOS: tile image build and `CATransaction` commit on a present thread |

## Isolation

`spk-env.sh` runs a command with `env -i`, a scratch `HOME`, scratch XDG
directories, and `PMUX_SOCKET=/private/tmp/pspk/run/prismattyc/pmux.sock`.
It never reaches the live pmuxd or the user's config, Spaces, or caches.

```sh
cargo build --release --locked -p prismattyc-host -p prismattyc-mux --bins --examples
mkdir -p /private/tmp/pspk/home /private/tmp/pspk/run /private/tmp/pspk/tmp /private/tmp/pspk/out
spike/render-thread/spk-env.sh pmux up &          # test pmuxd only
spike/render-thread/run-scenario.sh a-idle 200x60 6 20 -- /bin/zsh -f
spike/render-thread/run-scenario.sh b-yes 200x60 6 20 -- spike/render-thread/yes-after-3.sh
SPK_MORE="PRISMATTYC_SPIKE_PRESENT_THREAD=1" spike/render-thread/run-scenario.sh ...
```

`run-scenario.sh NAME CELLS WARMUP_S MEASURE_S -- HOST_ARGS` starts one
host, drops warm-up samples, measures, stops only that host, and prints the
table. `SPK_DURING` runs a command at the start of the measured window
(scenario d used it to `SIGSTOP` the test pmuxd by its verified pid).
`flood.py OUT` writes a fixed 32 MiB of 80-byte lines and records the elapsed
time.

## Probes

- `cargo run --release -p prismattyc-host --example render_thread_present_probe --locked -- bg|main`
  answers whether Core Animation can present from a non-main thread while
  main is blocked. Set `PROBE_CAPTURE_DIR` to also save two screenshots
  (needs Screen Recording permission).
- `cargo run --release -p prismattyc-core --example snapshot_handoff_bench --locked`
  times copy, pooled copy, double buffer, and per-row `Arc` handoff.

Raw tables from the runs in the design doc are in `results/`.
