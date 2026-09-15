# Rendering and transparency

The desktop application draws terminal cells and window controls into a
CPU framebuffer. A platform backend presents that image to the desktop.

| Platform or option | Presentation backend |
| --- | --- |
| X11 and XWayland | softbuffer. Transparency requires a depth-32 visual and a compositor. |
| Native Wayland with transparency | Shared-memory ARGB buffers. |
| macOS | Core Animation with premultiplied per-pixel alpha. |
| Optional `--gpu` | wgpu presentation. Per-pixel window transparency is not supported. |

`window_opacity` controls the default window background. Text and explicit terminal
background colors stay opaque on the alpha-capable presentation paths.
`chrome_opacity` controls window controls such as the tab strip and footer.

`window_blur` requests a desktop backdrop effect. It depends on the
platform and compositor. An opaque window background hides the backdrop.
See [configuration](config.md),
[macOS](macos.md#window-transparency-and-blur), and [Hyprland](hyprland.md).

## Optional GPU presentation

Build the host with the `gpu` feature and pass `--gpu`:

```bash
cargo run -p prismattyc-host --features gpu --locked -- --gpu
```

This option changes presentation. It does not move all terminal rasterization
to the GPU. Use the default backend when you need window transparency or blur.

## Native-window tests

The native fixtures under `tests/native/` exercise desktop rendering. Termwright
tests the nested terminal executable; it does not drive desktop windows.
A CPU framebuffer capture does not show the desktop compositor's backdrop.
