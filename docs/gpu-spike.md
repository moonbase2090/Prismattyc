# GPU raster spike — (D9)

**Status:** Spike complete (2026-08-13). Present-only prototype behind
`--features gpu --gpu`. Not a production backend.
**Depends on:** [decisions-v0.md](decisions-v0.md) D3 / D9; ADR-0006 idle
`ControlFlow::Wait`.
**Not:** a Phase 3 gate; PRD §5.6 / A-6 untouched.

## Question

Should `prismattyc-host` grow a GPU present/raster path now that the owner box has a
capable GPU? If yes: which API, what stays on the CPU, and what tickets follow?

## Hardware (this machine)

- NVIDIA AD103 GeForce RTX 4080, Vulkan instance 1.4.350
- Surfaces: Wayland + X11 (`VK_KHR_wayland_surface`, `VK_KHR_xlib_surface`)
- DRM nodes: `/dev/dri/card1`, `renderD128`
- Layers: NVIDIA Optimus, MangoHud (must not become a hard dependency)

This is a wgpu-first box. glow/OpenGL is a fallback API, not the primary.

## How paint works today

`prismattyc-host` rasterizes a full-window `u32` buffer on the CPU (`raster.rs`,
fontdue glyphs, chrome, overlays) and presents it with **softbuffer**. The
event loop is `ControlFlow::Wait` when idle. Light-cycle / pulse use
`WaitUntil` at quantized steps only. PTY bytes wake via `EventLoopProxy`.

The expensive parts on a large window are:

1. **Full-grid CPU raster** (every dirty paint walks cells × glyph blit).
2. **softbuffer present** (CPU copy into the compositor buffer).
3. **Animation ticks** (already bounded; not a poll loop).

A GPU backend that only replaces (2) is a **present** path. Replacing (1)
needs a glyph atlas and a cell-quad (or compute) pass. Pixel parity with the
CPU blit is the acceptance bar — same fontdue bitmaps and metrics.

## wgpu vs glow

| | wgpu | glow (OpenGL) |
|---|---|---|
| Owner box | Native Vulkan | Extra GL/EGL stack |
| winit 0.30 | First-class surface | Works, more platform glue |
| Wayland/X11 | One API | Two EGL stories |
| Fallback | Can select GL backend if Vulkan fails | GL only |
| Rich/canvas later | Compute + WGSL | Harder, less portable |
| Default binary | Optional feature; no hard dep | Same |

**Choice for a prototype:** wgpu. Keep glow as a documented escape hatch, not
the spike target. Matches D9 (“API choice open until a production ticket”).

## Prototype (this branch)

- Softbuffer remains the **default** present path. No GPU in default features.
- Opt-in: `--gpu` or `PRISMATTYC_GPU=1`. Without `--features gpu`, the flag errors
  with a rebuild hint (no silent no-op).
- With `--features gpu`: create a wgpu instance/adapter/device at window
  spawn; on failure, print and fall back to softbuffer (classic must still
  run).
- Glyphs stay CPU/fontdue. The GPU slice is **upload + nearest-neighbor
  present** of the existing `u32` buffer (`0x00RRGGBB` → BGRA8, `textureLoad`).
- Present mode prefers `AutoNoVsync` / `Immediate` so we do not block the
  event thread on vsync. We only submit when `host.dirty`.
- Idle: GPU path does not request redraws or present unless `host.dirty` (or
  a `WaitUntil` animation step). No per-frame tick. `ControlFlow::Wait` is
  unchanged.
- Timing: `PRISMATTYC_GPU_TIMING=1` prints per-paint wall time (raster + present).
- CI: `cargo check -p prismattyc-host --features gpu` so the feature does not rot.

Atlas / cell-quad raster is **out of this spike’s merge bar**. It is the first
follow-up if go.

## What might get faster (hypothesis, unmeasured until 88b)

| Workload | Present-only GPU | Atlas + cell quads |
|---|---|---|
| Large-window full repaint | modest (compositor copy) | yes, if damage stays full-frame |
| Many panes | little (still CPU per cell) | yes, if one atlas draw |
| Light-cycle / pulse | little (small dirty) | little |
| Scroll floods | little | yes (glyph reuse) |
| Idle | must stay Wait / zero GPU submit | same |

Measure on a **real display** (this box, `DISPLAY`/`WAYLAND_DISPLAY`), not
headless: CPU% of `prismattyc-host`, time inside `paint`, present time.
discipline: Wait must stay silent when nothing changes.

Live numbers from this spike (owner box, Wayland, debug/release as noted)
land in the Measurement section below when captured.

## Risks

- Driver / Optimus / Wayland vs X11 present modes
- Startup cost of wgpu adapter selection
- Binary size if wgpu is default-on (do not default-on)
- Power: a PresentMode that wakes every vsync would undo — use
  `AutoNoVsync` / on-demand present only
- Pixel drift if anyone “improves” glyph filtering on the GPU (shader uses
  `textureLoad`, not a filter)
- sRGB swapchain formats can gamma-encode the CPU buffer; we prefer
  linear `Bgra8Unorm` / `Rgba8Unorm`

## Go / no-go

**Conditional go** on a **present-only, feature-gated** path.

- Go: owner has Vulkan; D9 already planned this; softbuffer stays default;
  idle rule is clear; prototype compiles and inits on this box.
- Not go for “GPU raster replaces CPU” in this ticket — that needs
  measurement after present-only exists (PM-88b) and then an atlas (PM-88c).
- Not go for glow-first.
- Not go for default-on wgpu.

## Follow-up tickets

1. **PM-88a** — landed in this spike: wgpu present of the CPU `u32` buffer
   (`--features gpu --gpu`), Wait-silent idle, fallback on init failure.
2. **PM-88b** — measure present-only vs softbuffer on this 4080 (large window,
   6 panes, light-cycle, `yes` flood). Numbers in this doc. Pixel-compare a
   still frame if we can screenshot both paths.
3. **PM-88c** — fontdue glyph atlas + cell quads; golden vs CPU raster.
4. Later / not now: rich canvas, remote GPU (roadmap 9b).

## Measurement

Captured on the owner box during this spike. Debug vs release called out.
`PRISMATTYC_GPU_TIMING=1` is wall-clock around `paint()` (CPU raster + upload +
submit + present).

| Build | Path | Notes |
|---|---|---|
| debug, 2026-08-13 | `--features gpu --gpu` | NVIDIA GeForce RTX 4080 / Vulkan / `Bgra8Unorm` / `Immediate`; adapter+device+pipeline **294 ms** |
| debug | softbuffer (default) | unchanged; no wgpu in the binary without `--features gpu` |

Present-only vs softbuffer frame times (large window, 6 panes, `yes` flood) are **PM-88b**. This spike proves init + present path, not a speed win.

## Non-goals

- Requiring a GPU to run `prismattyc-host`
- Changing `prism` (nested classic)
- Claiming Phase 3 / §5.6 / production-rich
- Replacing Wait with Poll
