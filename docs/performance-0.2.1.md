# Scrolling and font memory in 0.2.1

Scrolling now moves row indices instead of copying the whole viewport.
Rows remain contiguous. Character writes use direct row access, and dirty
row updates set word-sized bit ranges. Screen snapshots keep their existing
logical row order and wire format.

The tab close button first uses a fitting glyph from the primary font.
It loads a fallback only when the primary font has no suitable glyph.
Terminal text still loads a covering fallback when needed. This avoids
expanding a large Nerd Font solely to draw a close button when another
primary font already has a suitable symbol.

## Measured effect

The following values are medians from three complete runs on the same host.
Each run used an Ubuntu 24.04 container with two CPUs and 4 GiB of memory,
Weston 13 with software OpenGL, an 80-by-24 viewport, 16-pixel DejaVu Sans
Mono, and 10,000 history rows. The baseline is the published Linux x86_64
0.2.0 binary. The candidate contains the changes above.

| Measurement | 0.2.0 | Candidate | Foot 1.16.2 |
| --- | ---: | ---: | ---: |
| ASCII output, MiB/s | 29.85 | 48.95 | 363.87 |
| Unicode output, MiB/s | 23.32 | 42.94 | 121.96 |
| Alternate-screen output, MiB/s | 60.51 | 60.54 | 369.74 |
| Sampled peak process RSS, MiB | 142.33 | 92.08 | 29.32 |
| Launch to first cursor-position reply, ms | 91.55 | 59.00 | 19.40 |
| Idle cursor-query p95, ms | 0.215 | 0.212 | 0.082 |

The workloads contain 64 MB of ASCII, 12.9 MB of Unicode, and 17.8 MB of
alternate-screen output. Output timing ends at a cursor-position reply.
It measures processing through the PTY, not display latency. The font memory
reduction depends on the primary font. A configuration that already uses the
large Nerd Font as its primary face still needs that font.

These results show a sustained scrolling improvement. They do not establish
parity with Foot. Foot retains a substantial throughput and memory advantage.
Short alternate-screen trials initially suggested a regression. Direct row
access removed the extra indexing work, and the longer workload above was
roughly level with the baseline.

## Reproduce and review

Use the [terminal comparison probe](../demo/performance/README.md).
It records binary hashes, dependency versions, raw samples, and failed cases.
Do not combine results from different renderers. Weston Pixman crashed
intermittently with the unchanged baseline; those runs were excluded as
failed experiments, and the complete comparison used software OpenGL.

The parser example isolates screen work from rendering. Its 80-row cases
improved most because a line feed no longer copies 80 rows of cells.
Thin LTO with one codegen unit did not improve that experiment, so the
release profile remains unchanged.

Correctness checks include mixed row rotations, overlapping copies, erases,
wide characters, grapheme clusters, state import/export, reflow, and damage
application. The font regression check confirms that a close button keeps
the unused fallback unloaded and that printing an actual Nerd glyph loads it.
Native UI, workspace, coverage, and mutation results belong to the associated
PR receipts; the benchmark alone is not a correctness gate.
