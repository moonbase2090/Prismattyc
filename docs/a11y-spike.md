# Host accessibility spike (PT-34)

**Status:** Spike complete (2026-09-01; probe retried the same day).
This document does not implement the adapter.
**Parent:** PT-34, “VoiceOver / screen-reader support for prismattyc-host.”
**Decision:** [ADR-0016](adr/0016-host-accessibility.md).
**Workspace baseline:** `0.1.158` for this PR.

The named target is macOS VoiceOver. This seat is Linux (KWin / Wayland).
The probe records what the OS APIs expose today. It then names the first
implementation slices.

## Question

Can `prismattyc-host` expose a live cell grid to VoiceOver or Orca through
OS accessibility APIs? If not, do we build an OS tree, a product speech
layer, or both?

## How paint works today

`prismattyc-host` opens a winit 0.30 window and software-rasters a full-window
`u32` buffer (`raster.rs`). softbuffer presents the pixels. Chrome (tabs,
handles, palette, splash, find, scrollbar, toasts) is paint, not toolkit
widgets. The window title is the only OS string the host already updates
(`window_title` in `main.rs`: tab and pane counts, scroll, unseen, mail
depth).

winit 0.30.13 has no AccessKit feature. The crate does not register an
AT-SPI application. macOS code (`macos_window.rs`, `macos_menu.rs`) talks
to AppKit for opacity, icon, and the dock menu only. It does not implement
`NSAccessibility`.

`decisions-v0.md` already freezes **A11y v0**: plain grid text is the
guaranteed textual surface. A rich attachment tree stays later.

## Live Linux probe (2026-09-01, second pass)

Seat: Arch Linux, KWin Wayland, `WAYLAND_DISPLAY=wayland-0`.
AT-SPI core `2.60.4` is installed. `python3` can import `gi.repository.Atspi`.
Orca is not installed.

The first pass saw an empty a11y bus because
`org.a11y.Status.IsEnabled` was **false**. Toolkit apps do not export
trees until that flag is true. The second pass set `IsEnabled=true`
(left `ScreenReaderEnabled=false`, so nothing spoke) and walked the
tree with `busctl` on `unix:path=/run/user/1000/at-spi/bus_1`.

Three `prismattyc-host` processes were running
(`/home/brandan/.local/bin/prismattyc-host`).

| Check | Result |
|---|---|
| `IsEnabled` before | `false` (bus had only portal proxies) |
| `IsEnabled` after | `true` (Plasma, KWin, krunner, … appear) |
| `org.a11y.atspi.Registry` well-known name | Still **fails to activate**. Walk each app’s `/org/a11y/atspi/accessible/root` instead. |
| `prismattyc-host` on the a11y bus | **Absent.** No host PID is a bus name. |
| Ghostty on the a11y bus | **Absent.** Same class of app: raster window, taskbar only. |
| Plasma taskbar button | `push button` name=`Prismattyc` description=`Activate Prismattyc;` — **zero children** |
| KWin Accessible tree | One unnamed `frame`. No client window contents. |

What a screen reader can hear **today**:

| Surface | Spoken name | Interior |
|---|---|---|
| Taskbar | “Prismattyc, push button. Activate Prismattyc.” | None |
| Host window | Nothing from the process | Pixel buffer |
| Empty pane | Same as prompt and Kiro banner | Silence |
| Prompt | Same | Silence |
| Kiro banner | Same | Silence |

The three named scenes are indistinguishable. Orca cannot read cells,
tabs, the palette, or mail. It can only activate the taskbar button
whose name is the window title the host already sets. The button name
on this seat was the short title `Prismattyc`, not the long
`Prismattyc — N tabs — M panes` form.

`IsEnabled` was set back to `false` after the dump.

## macOS VoiceOver (research; this seat cannot run VO)

winit’s AppKit handle is an `NSView`. VoiceOver announces the `NSWindow`
title. A custom view that only blits pixels is not an accessibility
element unless the process implements `NSAccessibility` (or an adapter
does). Empty pane, prompt, and banner are therefore the same as Linux:
title only.

AccessKit’s macOS backend implements that protocol. The public crate
`accesskit_winit` (winit `^0.30.5`) maps one AccessKit tree to
NSAccessibility, AT-SPI (`accesskit_unix`), and UI Automation.

## Why a cell-per-node tree fails

A typical pane is 80×24 or larger. One AccessKit node per cell floods
VoiceOver and Orca. Screen readers expect a document or text field with
a value and a caret, plus a small chrome tree.

Guest TUIs (nvim, Kiro) already speak to the screen reader when they run
**inside** an accessible terminal. Until this host is that terminal, they
are silent. After the grid becomes one document node, the guest’s own
speech stays out of scope (`pmux-attach` and nested classic stay out of
scope too).

## Decision

Use **both** an OS tree and a product announce channel. See ADR-0016.

Do not vendor another emulator’s accessibility code. Use AccessKit as a
library. Build the tree from Prismattyc state.

## Ordered follow-up ticket titles

Create these children of PT-34 after the ADR is accepted:

1. **AccessKit adapter and host chrome tree.**
   Window, tabs, badges, palette, splash, find, space rail, scrollbar.
2. **Focused-pane grid as one document node.**
   Visible lines as the value. Caret as the offset. No per-cell nodes.
3. **Live-region announces for mail, attention, and cursor line.**
   Speak changes. Do not make sound the only channel.

## Proof for later slices

- Unit-test the AccessKit tree snapshot. Do not require VoiceOver in CI.
- After the adapter lands, dump the AT-SPI tree on Linux for the three
  scenes above.
- Operator VoiceOver pass on macOS is the named-target proof.
