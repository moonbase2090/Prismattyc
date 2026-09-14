# Hyprland

Prismattyc runs natively on Hyprland (wlroots). This page lists what works,
what Hyprland does compositor-side, and the window rules worth copying.
Verified on Hyprland 0.55.4, nested and live sessions (PT-124).

## What works out of the box

- Native Wayland launch. `WAYLAND_DISPLAY` is set, no XWayland. `pmux space
  open` and `pmux attach --all` inherit the session environment and find the
  display.
- Decorations. Hyprland negotiates client-side decorations; winit draws a
  fallback title bar with close, maximize, and drag. You can move and close
  the window without any windowrule.
- Fractional scale. The host reloads the font at the new scale factor and
  refits the cell grid on `ScaleFactorChanged`. Text stays crisp at 1.5x and
  other fractional scales.
- Clipboard. Copy and paste use `arboard` over `zwlr_data_control_manager_v1`
  (wl-clipboard semantics). Primary selection and the regular clipboard both
  work.
- Keyboard. xkb layouts and IME preedit use the same winit path as other
  Wayland compositors.

## Transparency

Set `window_opacity` below `1.0` in `config.toml`. On Wayland the host
presents its own `wl_shm` `ARGB8888` buffers (PT-118), so the desktop shows
through the window ground, gaps, and padding. Text, cursor, badges, and
explicit SGR backgrounds stay opaque. Pane outlines and pane-local padding
follow pane opacity. See the support matrix in
[docs/config.md](config.md#transparency).

## Blur

`window_blur = true` has no effect on Hyprland: Hyprland does not implement
`ext-background-effect-v1`. Apply blur compositor-side with a window rule
instead:

```conf
# ~/.config/hypr/hyprland.conf
windowrulev2 = blur, class:^(prismattyc-host)$
# Optional: float and size the window.
# windowrulev2 = float, class:^(prismattyc-host)$
# windowrulev2 = size 60% 60%, class:^(prismattyc-host)$
```

Blur only shows where the window is translucent. Pair the rule with
`window_opacity` below `1.0`.

## Known limits

- The fallback title bar follows the window alpha. At low `window_opacity`
  the title bar is faint. The buttons still work.
- `window_blur = true` prints one startup notice on Hyprland because no
  client-side blur protocol exists there. Use the window rule above.
- Shared sessions size a pane to the smallest attached window. A larger
  attached window shows the pane at the top left and fills the remaining
  frame with the window ground at `window_opacity`. With an alpha-capable
  present path (always the case on Wayland) that frame is see-through
  rather than a solid border; this is the intended PT-87 look, not a
  sizing bug.
