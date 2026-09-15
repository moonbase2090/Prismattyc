# Compare terminal throughput and memory

This Linux probe runs two frozen `prismattyc-host` binaries and Foot.
Each case gets a private home directory, Xvfb display, and Weston compositor.
It does not connect to your desktop or your `pmuxd`.

## Run the comparison

1. Copy the baseline and candidate release binaries into `build/performance/`.
   Use separate files. Do not rebuild or replace them during a run.
2. Build the test image:

   ```bash
   docker build -t prismattyc-performance -f demo/performance/Dockerfile .
   ```

3. Run three trials with longer output workloads:

   ```bash
   docker run --rm --cpus=2 --memory=4g \
     -v "$PWD:/src" prismattyc-performance \
     python3 demo/performance/compare.py \
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
