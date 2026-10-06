# Present-cost spike harness (not for merge)

Reference code for `docs/design/present-cost.md` (issue #199). This branch is
never merged. Builds on the `spike/render-thread` harness in
`spike/render-thread/` (same isolation: `env -i`, scratch `HOME`, own pmuxd
socket under `/private/tmp/pspk`).

| Variable | Effect |
| --- | --- |
| `PRISMATTYC_SPIKE_PRESENT=iosurface` | macOS: present through three IOSurfaces on one layer instead of CGImage tiles |
| `PRISMATTYC_SPIKE_FLUSH=1` | `CATransaction::flush()` after each commit |
| `PRISMATTYC_SPIKE_TILE_DIFF=1` | Count dirty tiles whose pixels really changed |
| `PRISMATTYC_SPIKE_SKIP_UNCHANGED=1` | With the diff, present only changed tiles |
| `PRISMATTYC_SPIKE_DAMAGE_AUDIT=1` | With the diff, count undamaged tiles that changed (missed damage) |
| `PRISMATTYC_SPIKE_DAMAGE_STAGES=1` | Tiles each damage stage adds (compose, activity, bells, underlay, panes) |
| `PRISMATTYC_SPIKE_VERIFY=1` | Every 30th IOSurface frame: read back and compare with the premultiplied framebuffer; with `PRISMATTYC_DUMP_PRESENT`, also write `*.readback.png` and `*.atverify.png` |

Timing tables need `PRISMATTYC_SPIKE_TIMING=PATH` (see `spike/render-thread/README.md`).

```sh
cargo build --release --locked -p prismattyc-host -p prismattyc-mux --bins --examples
spike/present-cost/matrix.sh          # 120 probe runs -> /private/tmp/pspk/present/matrix.txt
spike/present-cost/setup-space8.sh    # test pmuxd + 8-session Space (fresh /private/tmp/pspk)
spike/present-cost/run-host.sh        # host scenarios -> /private/tmp/pspk/out/pc-*.final.txt
cargo run --release -p prismattyc-host --example premultiply_bench
```

`present_cost_probe BACKEND WxH SCALE DAMAGE THREAD [FRAMES]` measures one
case: `tiles`, `ring`, `inplace`, or `metal`; `full`, `band`, or `cell`;
`main` or `bg`. It never activates its window. Every run reads the
presented pixels back and reports `verify=pixel-identical` or a mismatch.

Raw output: `matrix.txt` and `results/`.
