# Compare terminal throughput and memory

## Measure checkpoint stalls

Run `checkpoint-latency.py` with a frozen release `pmuxd` binary:

```bash
python3 tests/performance/checkpoint-latency.py /path/to/pmuxd --panes 26 --seconds 20
python3 tests/performance/checkpoint-latency.py /path/to/pmuxd --panes 26 --seconds 20 --persist off
```

Each pane fills 10,000 history rows and then emits one byte every 50 ms.
The probe measures control-socket round trips across repeated checkpoints.
It uses a private daemon and generated text. It does not read existing panes.
Run baseline and candidate sequentially while compilation is idle.
Compare the maximum pause and the count above 50 ms as well as percentiles.
Rare checkpoint stalls can fall outside the 99th percentile.
This probe measures control latency, not compositor presentation latency.

The Linux `blocked_checkpoint_writer_does_not_block_control_requests` test
blocks a real checkpoint write with a small FIFO. It checks control replies
and PTY echo while the write is blocked. Set `PMUX_CHECKPOINT_TEST_BINARY`
to an older daemon when checking that this regression detects the old bug.

## Measure output throughput

This Linux probe runs two frozen `prismattyc-host` binaries and Foot.
Each case gets a private home directory, Xvfb display, and Weston compositor.
It does not connect to your desktop or your `pmuxd`.

## Run the comparison

1. Copy the baseline and candidate release binaries into `build/performance/`.
   Use separate files. Do not rebuild or replace them during a run.
2. Build the test image:

   ```bash
   docker build -t prismattyc-performance -f tests/performance/Dockerfile .
   ```

3. Run three trials with longer output workloads:

   ```bash
   docker run --rm --cpus=2 --memory=4g \
     -v "$PWD:/src" prismattyc-performance \
     python3 tests/performance/compare.py \
       --baseline /src/build/performance/baseline-host \
       --candidate /src/build/performance/candidate-host \
       --output /src/build/performance/comparison \
       --trials 3 --scale 20
   ```

4. Read `comparison/summary.json` and each case's `samples.json`.
   Use medians across all trials. Keep failed runs with their logs.

The output directory must not exist. The runner stops on a failed case.
It records binary hashes, Foot and Weston versions, renderer, and workload scale.
Keep compilation, coverage, and other CPU-intensive jobs idle during measurement.

## Interpret the results

Both terminals use an 80-column, 24-row viewport, a 16-pixel DejaVu Sans Mono
font, and a 10,000-row history limit. Each workload ends with a cursor-position
query. Its reply proves that the terminal processed the preceding output.
It does **not** measure input-to-display latency or prove that every intermediate
frame reached the display. Use native captures and interaction tests for those
properties.

The runner samples terminal-process RSS, PSS, private memory, and CPU ticks.
These values exclude the workload child and compositor. Peak RSS is sampled,
so a brief peak between samples can be missed. The Unicode workload can load
additional fonts. Compare the same phases and retain the font logs.

Weston uses software OpenGL by default. The Ubuntu 22.04 and Ubuntu 24.04
Pixman configurations intermittently crashed with the unchanged 0.2.0 binary
during development. Those failed runs are not benchmark results. Use
`--renderer pixman` only when investigating that path. Do not combine results
from different compositors, renderers, or workload scales.

## Isolate parser and screen work

Run the parser example without a PTY or renderer:

```bash
cargo run --release --locked -p prismattyc-emulator --example throughput
```

It compares 24-row and 80-row viewports with history disabled and enabled.
Build both revisions with the same toolchain and profile. Copy the example
into an older checkout if it predates this probe. This microbenchmark does not
replace the native comparison or correctness tests.

## Profile emulator grid and history

Run the fixed-input profile from a clean, frozen checkout:

```bash
python3 tests/performance/profile-grid-history.py \
  --output build/performance/grid-history-baseline
```

The output directory must not exist. The runner builds the locked release
probe, records its SHA-256, source revision, machine, OS, and Rust toolchain,
then runs five independent timing processes per case. The default feed cases
each replay 32 MiB in 8 KiB chunks; reflow performs 64 resize operations. Raw
measurements are written to `timings.jsonl`. The starting case rotates each
round to limit timing drift from a fixed case order.

Cases are `ascii` (plain lines), `scroll` (one-character lines with history
disabled), `history` (steady eviction with a warmed 10,000-row scrollback),
`unicode` (wide characters, combining marks, and joined emoji), `sgr` (styled
text), and `reflow` (alternating 80x24 and 96x30 with warmed 10,000-row
history). The reflow input uses 34-column hard-broken ASCII lines, so it does
not measure soft-wrap-heavy reflow. Setup and warm history are outside the
timed interval. Feed timing covers `Emulator::feed`; reflow timing covers
`Emulator::resize`. The result
JSON records input size, duration, throughput where applicable, final cursor,
retained history, and allocated history bytes.

Collect separate CPU samples with the same binary and inputs:

```bash
python3 tests/performance/profile-grid-history.py \
  --output build/performance/grid-history-profile \
  --profile --profile-units 512 --sample-seconds 3
```

On macOS, the runner attaches `/usr/bin/sample`; on Linux it uses `perf
record` and saves a text report from `perf report`. Linux may require the host's
normal perf permissions. Profile repetitions are separate from timing runs and
default to 512 MiB per feed case and 1,024 resizes. Keep other CPU-heavy work
idle. The runner fails if a workload exits too soon to cover the requested
sample duration; increase `--profile-units` for slower hosts or longer samples.
Keep the manifest, raw timing rows, and sample reports together when
comparing revisions. Sample percentages are profiler samples, not instrumented
wall time, and inclusive parent percentages must not be added to child rows.
