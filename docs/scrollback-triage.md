# Scrollback and selection triage

This note separates two similar-looking scroll failures. Alternate-screen
input ownership is confirmed. A separate split-pane routing defect is now
identified and fixed in the host source; the original pointer position was not
captured, so its role in the earlier report cannot be proven retrospectively.

## Confirmed cause: alternate-screen panes had no host scrollback

On 2026-09-25, `pmux render-status --json` showed the focused Prismattyc pane
with `alt_active: true`, `view_scroll: 0`, and a successful recent raster
presentation. That is a healthy render with the child using the alternate
screen, rather than evidence of a frozen repaint.

The host previously exposed primary-screen history only:

- [`Screen::max_view_scroll`](../crates/prismattyc-core/src/lib.rs) returned
  zero while the alternate screen was active.
- [`pan_view_scroll` and `set_view_scroll`](../crates/prismattyc-host/src/main.rs)
  reset the host offset to zero in that mode.
- [`guest_alt_blocks_host_select`](../crates/prismattyc-host/src/main.rs)
  blocks ordinary host selection for guest alternate-screen applications
  (including those using mouse tracking); Shift bypasses the pointer-selection
  block. Keyboard selection and host copy commands also follow the guard.
- The mux already retained alternate-screen history for `pmux-attach`, but the
  native host did not enable or use that history.

The old policy made scrolling and copying appear broken without an explanation.
A report that Grok scroll works in `/minimal` but not `/fullscreen` is
consistent with the fullscreen UI using the alternate screen; this remains an
inference until the pane mode is captured during reproduction.

## Host scrollback fix

The host now opts into the core's bounded alternate-screen row history. Its
viewport, scrollbar, and selection/copy paths use that history while retained
rows exist. Shift+wheel is host-owned; an unmodified wheel over a mouse-aware
TUI still goes to the child. Shift-drag selection continues to auto-copy.

This preserves rows the TUI actually scrolls off the top of its screen. It
cannot recover text that the TUI erases or overwrites during a full-screen
redraw, and scrolling inside a partial scroll region does not add rows to the
host history. That content needs a TUI-provided history or a separate screen
snapshot/transcript feature.

## Current Prismattyc session: alternate-screen input owns the wheel

On 2026-09-26, after the host reopened, the active `prismattyc-1` pane reported
`alt_active: true`, host `view_scroll: 0`, and a successful recent raster
presentation. A read-only mux snapshot also reported `alt_active: true`,
`max_view_scroll: 0`, and child mouse tracking enabled. At the time of capture,
the host had no history to pan; over the pane, wheel input was routed to the
child TUI. If the TUI did not visibly move its own content, the gesture looked
frozen even though the host was repainting normally. This is the alternate-
screen policy, not the unfocused-split routing defect below.

Ordinary host selection and copy shortcuts are also blocked in this mode.
Shift-drag can select and copy visible text, but cannot reveal earlier frames
that the alternate screen did not retain.

## Confirmed code defect: wheel over an unfocused split used focused-pane state

On 2026-09-26, live diagnostics showed Scorecard-1 on the primary screen with
5,691 history rows and mouse tracking off. Scorecard-2 was focused, on the
alternate screen, with mouse tracking on. Both host offsets were zero. The
older wheel handler built its routing decision from the focused pane, even
when the pointer was over the other split. In this state, a wheel over
Scorecard-1 could be treated as input for Scorecard-2 and consumed instead of
scrolling Scorecard-1. This is a confirmed code defect; the old incident's
pointer position was not recorded, so it is a strong explanation rather than
proof of that particular gesture.

The host now routes a wheel over a non-focused split using that pane's own
mouse mode, screen mode, and scroll offset. Host scrolling updates that pane
without changing keyboard focus. If the pane owns mouse-wheel input, the event
goes to that pane's child instead of the focused sibling.

Scorecard-2 remains a separate alternate-screen case. Its captured mux snapshot
had no retained history rows and reported mouse tracking on, so Prismattyc sent
wheel input to the TUI. The host fix above makes future top-of-screen scrolls
available through Shift+wheel; it cannot restore rows that scrolled before the
host began retaining them or recover overwritten TUI frames.

The host also blocks ordinary selection and copy shortcuts for guest
alternate-screen applications. Shift-drag bypasses the pointer-selection
block and finishing a selection auto-copies it. If Shift-drag still fails,
capture that pointer and clipboard path separately from scroll routing.

The earlier zoom recovery may have reset a pane's local scroll offset when
emulator row or column counts changed. It may also have caused the TUI to
repaint. Neither explanation is established by the recorded state. The
`render-status` snapshot captured after the fix should confirm whether the
non-focused primary split's local offset changes under the pointer.

## Read-only diagnostics

Capture host and server state before zooming or changing the pane mode:

```bash
pmux render-status --json
pmux attach SESSION --styled-json --read-only
```

`render-status` contains the native host's pane mode and `view_scroll`.
`pmux attach --styled-json --read-only` reports mux/server history and mode;
its `view_offset` is not the native host's local offset. Do not use the attach
snapshot alone to diagnose where the GUI pane is in its history.

For code-level follow-up, capture the same host status before and after a
failed wheel gesture, drag selection, and resize. Check alternate-screen and
mouse-tracking state first, then compare the host scroll offset and repaint
status. This separates the established mode policy from a possible stale
input or viewport state in the Scorecard case.
