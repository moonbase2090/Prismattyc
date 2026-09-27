# SIMD plain-text scan profile

## Decision

Do not add a production SIMD scanner yet. On the fixed ASCII `Emulator::feed`
workload, VTE's `Parser::advance` accounts for 4.4% of recorded leaf samples.
The screen and history path accounts for about 80%. A sample-based Amdahl
estimate that removes every `Parser::advance` sample gives a 1.046x ceiling
for this workload, calculated as `1 / (1 - 0.0442)`. A scanner would leave
some parser work in place, so its actual ceiling is lower. This estimate is
not a measured speedup. The ceiling also falls well below the 1.2x target for
an optimization pass.

The current parser also owns state that an external scanner cannot inspect.
`vte::Parser` does not expose whether it is in ground state, inside a control
sequence, or holding partial UTF-8. `StreamParser` tracks only UTF-8
continuations. Bypassing VTE safely would need a new state contract or a second
state machine. That cost is hard to justify against a 4.4% parser share.

## Machine and build

| Item | Value |
| --- | --- |
| Source revision | `204a71f` |
| Machine | Apple M5 Max, 18 logical CPUs, ARM64 |
| OS | macOS 27.0, build `26A428` |
| Rust | `rustc 1.98.1`, LLVM `22.1.8`; Cargo `1.98.1` |
| Build | Release, `--locked`, default target features |
| SDK | macOS 26.5 SDK via `SDKROOT`; deployment target 15.0 |
| Probe SHA-256 | `c687296c73c3f3057c3a5598bf8847f6753d2caf668b6068ce617e864b7e953d` |
| Host SHA-256 | `1cd324d1470e0964b90b920d45409da57aeaa1373737287279d65114841e61c1` |
| Daemon SHA-256 | `3e57d87c878dc4b6d949b410d4585a05b3508f31c95cdd6bc30a69bcab7bfc5d` |

The active Command Line Tools are selected at
`/Library/Developer/CommandLineTools`. The macOS 27 SDK linker mismatch noted
in the workspace instructions prevented using SDK 27, so these release builds
used the installed 26.5 SDK. No source code changed for this report.

## Fixed-input ingestion results

The repository has no Criterion or Divan benchmark for this path. I used the
existing `profile_feed` fixed-input probe documented in
[`tests/performance/PROFILE.md`](../../tests/performance/PROFILE.md). Each
process warms 10,024 history lines before timing `Emulator::feed`. The timed
input is fed in 8,192-byte chunks to an 80-column, 24-row emulator with a
10,000-row history limit. Each case ran in five independent release processes.
The trial order rotated between cases.

The probe includes parser work, graphics protocol scanning, screen mutation,
damage tracking, and history updates. It does not measure native rendering.
Each run retained 29,050,000 history bytes against a 100,663,296-byte budget.
The cell size was 36 bytes. Peak RSS was stable across five runs per case.

| Input | Bytes | Trial times (seconds) | Median throughput (MiB/s) and range | Peak RSS |
| --- | ---: | --- | --- | ---: |
| ASCII lines | 19,200,000 | 0.213307, 0.185867, 0.185611, 0.184974, 0.186729 | 98.514 (85.841–98.990) | 54,984,704 B |
| SGR escapes | 9,900,000 | 0.129423, 0.128662, 0.128800, 0.128657, 0.129369 | 73.302 (72.950–73.384) | 45,760,512 B |
| Unicode lines | 12,900,000 | 0.124858, 0.124287, 0.123373, 0.124030, 0.125824 | 98.984 (97.775–99.717) | 48,709,632 B |

The first ASCII run is much slower than the other four. It remains in the raw
range and does not affect the reported median. No candidate implementation was
built, so these values are baseline measurements, not before-and-after claims.

## Sampled CPU profile

I collected five one-second `/usr/bin/sample` profiles per input using a
3,000,000-repetition run of the same probe. The table groups non-overlapping
top-of-stack samples. `grid/history` includes screen writes, line feeds, cell
width and history operations, plus the associated memory moves. `Parser::advance`
is its exclusive sampled share, not its inclusive call-tree share.

| Input | Leaf samples | Grid and history | `Emulator::feed` self | VTE `Parser::advance` self | Other named work |
| --- | ---: | ---: | ---: | ---: | --- |
| ASCII lines | 3,732 | 79.96% | 11.92% | 4.42% | Graphics APC 3.48%, other 0.21% |
| SGR escapes | 3,612 | 50.19% | 9.16% | 7.23% | SGR 8.06%, UTF-8 conversion 6.23%, allocation 11.85%, Graphics APC 2.74%, other 4.54% |
| Unicode lines | 3,678 | 72.97% | 11.07% | 3.97% | UTF-8 conversion 3.56%, Graphics APC 3.34%, other 5.08% |

The standalone `core::str::from_utf8` leaf samples were 0% for ASCII, 6.23%
for SGR escapes, and 3.56% for Unicode. They do not represent all UTF-8 decode
work. VTE's ground-state path calls `str::from_utf8`, and the compiler can
inline parts of that work into `Parser::advance`. The escape workload's
`from_utf8` samples are in SGR parameter parsing, which a plain-text scanner
would not remove.

The locked `vte` version is 0.15.0. Its ground-state parser already uses
`memchr` to find ESC, then validates the preceding slice as UTF-8 before
dispatching printable characters. The proposed scanner would duplicate part of
that work unless it could safely bypass the parser and its callbacks.

## Native terminal workloads

I ran the release host and daemon on macOS with per-frame `render_timer` logs.
The daemon received these workloads:

| Workload | Input and activity | Sample duration | Frames | Raster median / p90 | Present median / p90 |
| --- | --- | ---: | ---: | ---: | ---: |
| `cat` | 16,500,000-byte plain-text file, 250,000 lines | 3 s | 561 | 3,093 / 3,191 µs | 6,534 / 6,758 µs |
| `ls -R` | 100 passes over `crates/prismattyc-emulator` and `crates/prismattyc-core`; saved history held 341,091 bytes and 4,211 lines | 3 s | 58 | 2,772 / 4,175 µs | 6,787 / 9,615 µs |
| Cargo build | Verbose release build of `profile_feed` in a fresh target directory; build completed in 3.90 s and saved output held 71,280 bytes and 880 lines | 5 s | 116 | 3,652 / 3,748 µs | 7,160 / 7,311 µs |
| Vim scroll | 30,000-line file and 60 Ctrl-F inputs | 3 s | 16 | 4,554 / 7,584 µs | 9,710 / 27,658 µs |

The `cat` daemon sample captured the active path through
`Emulator::feed`, `StreamParser::advance`, VTE `Parser::advance`, and screen
callbacks. These short native samples confirm that the parser runs on actual
PTY output, but they do not give a stable parser percentage for every workload.
For the Vim run, a saved screen changed from `line 000000` at the top to
`line 001260` after 60 Ctrl-F inputs, confirming that Vim scrolled.
The frame counts include startup and repaint frames. The `parse_us` renderer
field records only the latest drain call, not total parser time between frames;
I did not use it to estimate parser share. `present_us` measures the presenter's
call duration, not display or vsync latency.

PT-246's `render-bench.sh` requires an X11 test environment with Xvfb and
`xdotool`, so I did not run that Linux harness on this Mac. The native host used
the same per-frame `render_timer` counters. This is a macOS rendering sample,
not a full PT-246 run or a platform comparison.

## Method limits and reproduction

The emulator probe is an existing fixed-input timing tool, not a Criterion or
Divan benchmark. I did not add a benchmark dependency or claim a speedup without
a candidate. The report's profile percentages are sample shares, not
instrumented time. The native renderer measurements are separate runs and must
not be combined with the emulator shares as one end-to-end percentage.

Build the probes from this revision with:

```bash
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk \
MACOSX_DEPLOYMENT_TARGET=15.0 \
cargo build --release --locked \
  -p prismattyc-emulator --example profile_feed \
  -p prismattyc-host -p prismattyc-mux --bins
```

Run each timing case five times, rotating case order:

```bash
target/release/examples/profile_feed ascii 10000 24 8192 300000
target/release/examples/profile_feed escapes 10000 24 8192 300000
target/release/examples/profile_feed unicode 10000 24 8192 300000
```

For a symbolized sample, start a separate process with 3,000,000 repetitions
and run `/usr/bin/sample <pid> 1 -file <report>`. Samply 0.13.1 also recorded a
profile, but its report was not symbolicated in this environment. `xctrace`
could not record because this machine has Command Line Tools rather than full
Xcode.

The results do not support adding SIMD on the current boundary. Revisit only if
a repeatable profile shows that safe, ground-state parser work is large enough
to outweigh the state and correctness costs.
