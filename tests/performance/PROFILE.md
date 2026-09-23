# Profile parser and screen work

Build the probe from a frozen checkout using the same toolchain and release
profile as the binary being investigated:

```bash
cargo build --release --locked -p prismattyc-emulator --example profile_feed
```

Arguments are workload, history row limit, viewport rows, feed chunk size,
and input repetitions. The viewport is 80 columns. Workloads are `ascii`,
`wrapped`, `unicode`, and `escapes`.

```bash
target/release/examples/profile_feed ascii 10000 24 8192 300000
target/release/examples/profile_feed unicode 100000 80 8192 300000
```

Run at least five independent processes per case and alternate case order.
Record the binary hash, source revision, toolchain, machine, and process
memory measurements. Keep other builds and performance jobs idle.

The timed interval covers `Emulator::feed`, including parser, graphics
protocol scanning, screen mutation, damage bookkeeping, and history work.
It excludes input construction and history warmup. Process peak RSS includes
those allocations. Throughput is input MiB per second, so different workloads
are not equivalent amounts of terminal work.

The JSON records retained history bytes and actual history lines. A requested
row limit can exceed the byte budget; use the measured retained allocation
when comparing history with CPU cache size. The last viewport's text and
cursor are also recorded for comparisons across chunk sizes. These receipts
do not check every style or history cell.

## Inspect UTF-8 chunk boundaries

The independent parser/emulator probe prints each split of several small
Unicode inputs. `parser_equal` reports whether the parser emitted the original
characters; `screen` records the emulator result.

```bash
cargo run --release --locked -p prismattyc-emulator --example utf8_chunk_probe
```

Inspect the JSON results. A successful process exit only means the probe ran;
it does not mean every split matched. Resolve discrepancies before treating
Unicode throughput numbers as proof of correct processing.

## Separate rendering from ingestion

Use the native host with the same font, viewport, output, and presenter in
each trial. Compare default rendering and `font_ligatures = true` separately.
Include ASCII floods, Unicode output, paced short styled runs with repeated
and changing text, and idle intervals. Measure both one and multiple panes.
A cursor-position reply proves ingestion through the query; it does not prove
that every intermediate frame was displayed.

Collect CPU samples in separate runs from timing trials. Attribute samples
to screen mutation, parser/protocol scanning, shaping, glyph rasterization,
blending, and allocation. Avoid adding overlapping inclusive call-stack
percentages. Renderer log durations are not a complete CPU accounting system;
in particular, the stored parse duration is the latest drain call, not an
accumulated total across all drains before a frame.

Native X11 measurements do not establish macOS or Windows performance, GPU
presentation cost, or real display latency. Record those environments
separately before making platform-wide claims.
