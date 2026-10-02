# Emulator grid and history profile

## Summary

The dominant costs vary by workload. Ordinary ASCII output spends most sampled
CPU time in cell writes, short-line scrolling spends most in `Screen::line_feed`,
and resize reflow spends most in reflow packing and screen reconstruction. The
VTE parser itself remains a small part of the plain ASCII profile. This points
to separate, workload-specific follow-ups rather than one general fast path.

No optimization was implemented for this profile. The impact ranges below are
targets for follow-up experiments, not measured speedups.

## Machine and method

| Item | Value |
| --- | --- |
| Source revision | `4a83343af53926477e80aab5ee7113094438bf91` (`origin/main`) |
| Machine | Apple M5 Max, ARM64, 18 logical CPUs |
| OS | macOS 27.0, build `26A428` |
| Rust | `rustc 1.98.1`, LLVM `22.1.8`; Cargo `1.98.1` |
| Build | Locked release profile, default target features |
| Probe | `profile_grid_history`; SHA-256 `0bc2325fea2bc4d248fb891d219405101666af9376268571f7dede82d7fdadd9` |
| Cell size | 36 bytes |

The fixed-input probe and runner are documented in
[`tests/performance/README.md`](../../tests/performance/README.md). Each feed
timing process replayed a fixed 32 MiB input in 8 KiB chunks. The runner made
five independent processes per workload. CPU profiles came from separate
three-second `/usr/bin/sample` runs of the same release probe with 512 MiB of
feed input (1,024 resize operations for reflow). Timing excludes input
construction and history warm-up. Feed measurements include `Emulator::feed`
and its parser, screen, damage, and history work. Reflow measurements include
`Emulator::resize` against 10,000 warmed history rows. Sampling and timing runs
were sequential, with other CPU-intensive work idle. The runner rotates the
starting workload each timing round and rejects sample runs whose timed work
does not outlast the requested sampling window.

The history workload began with 10,000 retained rows and kept that depth during
the timed feed. Its retained row and deque allocations used 29,050,000 bytes.
Reflow alternated an 80x24 screen and a 96x30 screen with the same warmed
history. Its 34-column ASCII lines are hard-broken and do not soft-wrap at
either width, so the result describes short-line packing rather than
soft-wrap-heavy reflow. Results are from one ARM64 Mac and are not a platform
comparison.

## Timing results

Each cell gives the median across five processes and the full observed range.
Throughput is input MiB/s; workloads do different amounts of terminal work per
input byte, so use it only within the same fixed workload. Reflow is reported
per resize because it has no input stream.

| Workload | Timed work | Median | Range |
| --- | --- | ---: | ---: |
| ASCII | 32 MiB of plain lines, no history | 107.87 MiB/s | 104.09–110.14 MiB/s |
| Short-line scroll | 32 MiB of `x\r\n`, no history | 31.16 MiB/s | 29.68–31.26 MiB/s |
| Full scrollback | 32 MiB of lines, 10,000-row history | 96.71 MiB/s | 94.85–97.90 MiB/s |
| Unicode | 32 MiB of wide, combining, and joined emoji text | 114.67 MiB/s | 113.31–115.23 MiB/s |
| SGR | 32 MiB of styled text | 90.39 MiB/s | 85.18–91.22 MiB/s |
| Reflow | 64 resizes with 10,000 history rows | 3.475 ms/resize | 3.426–3.545 ms/resize |

The table records baselines only. In particular, the input-byte throughputs
cannot be compared as if each byte caused equal amounts of screen work: the
short-line case issues one line feed per three bytes, while other cases write
many more characters between line feeds.

## Sampled hot spots

The table reports top-of-stack sample counts from `/usr/bin/sample`; the
percentages use the main thread's total sample count for that workload. These
are exclusive sample buckets. The call graph also has inclusive parent counts;
do not add those parent counts to this table. Rows are rounded to one decimal
place.

| Workload (total samples) | Largest exclusive sample buckets |
| --- | --- |
| ASCII (2,316) | `Screen::put_char` 1,007 (43.5%); `clear_wide_pair_covering` 407 (17.6%); `Emulator::feed` 280 (12.1%); `Screen::line_feed` 248 (10.7%); VTE `Parser::advance` 85 (3.7%) |
| Short-line scroll (2,298) | `Screen::line_feed` 1,681 (73.2%); `_platform_memmove` 134 (5.8%); `Screen::put_char` 95 (4.1%); `Emulator::feed` 85 (3.7%); `CellGrid` range indexing 34 (1.5%); damage range updates 31 (1.3%) |
| Full scrollback (2,291) | `Screen::put_char` 874 (38.1%); `clear_wide_pair_covering` 359 (15.7%); `Screen::line_feed` 303 (13.2%); `Emulator::feed` 272 (11.9%); `_platform_memmove` 128 (5.6%) |
| Unicode (2,310) | `Screen::put_char` 613 (26.5%); `Screen::line_feed` 286 (12.4%); `Emulator::feed` 243 (10.5%); `clear_wide_pair_covering` 226 (9.8%); `char_display_width` 193 (8.4%); cluster hashing 151 (6.5%) |
| SGR (2,219) | `Screen::put_char` 343 (15.5%); allocator `mach_absolute_time` 320 (14.4%); VTE `Parser::advance` 216 (9.7%); `Emulator::feed` 208 (9.4%); `Screen::line_feed` 205 (9.2%); `apply_sgr` 66 (3.0%) |
| Reflow (2,265) | `Screen::resize_impl` 1,037 (45.8%); `reflow::pack` 915 (40.4%); `char_display_width` 96 (4.2%); allocator and free routines account for additional samples |

The ASCII result agrees with the prior parser investigation: the parser's
exclusive share is 3.7%, while cell writes and wide-pair checks together are
61.1%. The full-history sample shows scrolling and row movement but does not
attribute a large exclusive share to `retain_scrolled_row`; the current code
already reuses the oldest row allocation when history is full.

The SGR call graph shows `apply_sgr` constructing a nested
`Vec<Vec<u16>>` from VTE parameter groups. Samples include vector collection,
allocation, and freeing beneath that path. This is a concrete allocation
target for styled workloads, but its cost does not apply to plain output.

The reflow profile is dominated by `Screen::resize_impl` and `reflow::pack`.
Together they account for 86.2% of exclusive samples in this resize-only
workload. The source path copies cells into logical lines, repacks them, and
builds replacement grids and history queues; the profile cannot distinguish
which copy or allocation should change without a focused follow-up experiment.
Because the input lines do not soft-wrap, this profile does not establish the
cost of reflowing long logical lines.

## Ranked follow-up candidates

Ranks combine measured sample share with how broadly a path is exercised. The
expected impact is a hypothesis for the named workload and must be replaced by
before/after measurements in a follow-up.

1. **Narrow-cell write path:** reduce per-character work for ordinary width-1
   cells, starting with the wide-pair checks around `put_char`. `put_char` and
   `clear_wide_pair_covering` contribute 61.1% of ASCII top-of-stack samples.
   Target a 5–15% reduction in the plain ASCII feed time; measure Unicode and
   overwrite behavior to guard wide-cell correctness.
2. **Short-line scroll updates:** investigate the row fill, wrapped-row flags,
   damage updates, and epoch work in `Screen::line_feed`. The function accounts
   for 73.2% of the short-line scroll samples, with another 8.6% in row
   movement, indexing, and damage helpers. Target a 10–25% reduction for this
   scroll-heavy workload; report impact on ordinary ASCII separately.
3. **Reflow staging and packing:** reduce repeated cell copying or temporary
   allocation in resize reflow, using the 10,000-row, short hard-broken-line
   case as the initial regression workload. The core resize and pack functions
   account for 86.2% of samples in that resize-only profile. Target a 20–40%
   lower time for this case; add a long-line, soft-wrap-heavy case before
   generalizing the result to other reflow workloads.
4. **SGR parameter storage:** avoid or reduce nested parameter-vector
   construction in `apply_sgr` while preserving colon and semicolon color
   behavior. The SGR path is 90.39 MiB/s in the baseline, and its profile
   shows allocator work and parameter-vector collection below this code path.
   Target a 5–15% reduction in this SGR workload; plain text should be
   unchanged.
5. **Full-history row retention:** test whether outgoing row copies or
   scrollback metadata still merit a change after the existing allocation
   reuse. The 10,000-row profile has 13.2% in `line_feed` and 5.6% in
   `memmove`, but no separately dominant history helper. Treat this as a lower
   confidence candidate and target no more than a 3–8% improvement until a
   focused profile establishes a larger share.

## Reproduction

Run five timing trials and retain their manifest and JSONL output:

```bash
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX15.4.sdk \
python3 tests/performance/profile-grid-history.py \
  --output build/performance/grid-history-baseline
```

The SDK override was needed for this machine's active macOS 27 Command Line
Tools linker. On a compatible toolchain, omit `SDKROOT`. To collect the sampled
profiles separately:

```bash
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX15.4.sdk \
python3 tests/performance/profile-grid-history.py \
  --output build/performance/grid-history-profile \
  --profile --profile-units 512 --sample-seconds 3
```

The runner refuses to overwrite an output directory and checks that each
timing run and profile run produces the same final visible text and history
receipt. The raw sample reports and JSON results are kept together under the
chosen output path, which is ignored by Git. The sample percentages are
profiler estimates, not instrumented time.
