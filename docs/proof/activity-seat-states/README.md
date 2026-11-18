# Activity monitor seat states

Renders of the activity monitor for the one-rule seat state change, for visual review.

A scene harness fed one scripted fleet into the monitor's own model (`Host::apply` with
metadata-only pages and a pmuxd session snapshot), rendered it with the monitor's real
Lunatui render path (`Monitor::frame`), encoded the grid with Lunatui's `Encoder`, and
rasterized the bytes with agg. These are not captures of a Prismattyc window. The same
harness ran on main for `before-wide.png`.

The fleet holds the case that prompted the change: `builder`'s hooks report a tool 30 s ago
while its pane has printed nothing for 9 minutes, so pmuxd's snapshot calls it stuck.

| File | Shows |
| --- | --- |
| `before-wide.png` | main: `builder` reads `! stuck for 0s` beside a working lane; waiting rows are stale at 30 s |
| `after-wide.png` | this change: `builder` is working for 9m; `tester` is stuck (turn open, 6 quiet minutes); `docs-writer` settled to idle; a source column; `no events` instead of a snapshot guess; only the disconnected host is stale |
| `after-narrow.png` | 60 columns: no source column, the host heading names the source |
| `after-details.png` | details for `builder` |
| `seat-states.gif` | 3 s per scene: before, then wide, medium, narrow, small and details |
