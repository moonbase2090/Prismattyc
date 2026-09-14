# Spike: click hyperlinks in prismattyc-host

**Status:** implemented. The first slice landed in PR #204 (2026-08-22):
auto-detect `http(s)` + Ctrl/Cmd+click + `xdg-open`/`open`. Package 0.1.39
adds OSC 8 parsing and cell targets. This feature is not a fidelity claim.
**Depends on:** [ADR-0001](adr/0001-host-selection-clipboard.md) (selection);
[ADR-0003](adr/0003-hybrid-mouse.md) (hybrid mouse).
**Not:** rich-surface links; TTY `prism-mux-attach` (the outer terminal already
clicks painted text).

## Question

The owner clicks a printed `https://…` URL in `prismattyc-host` (example:
a GitHub App install link) and nothing happens. How should Prism open
that URL, without stealing selection or app-mouse?

## RCA

Left-click in `prismattyc-host` is **selection** (ADR-0001) or **child SGR/X10
report** (ADR-0003). There is no URL hit-test and no `xdg-open` / `open`
path. `crates/` has zero OSC 8 parse or hyperlink cell attribute.

The screenshot is **plain text** in the grid, not an OSC 8 run. Ghostty and
Kitty auto-detect `https://` in the viewport and open on a modified click.
Prism does not.

`prismattyc-mux attach SESSION` over SSH paints into the outer Mac terminal. That
outer host can already click the painted URL. This spike is **windowed
`prismattyc-host`**.

## Decision (proposed)

Two layers, same click gesture.

| Layer | Source | First slice? |
|---|---|---|
| Auto-detect | `http://` / `https://` in the visible grid (and scrollback view) | Yes |
| OSC 8 | `OSC 8 ; params ; URI ST` on cells, as VTE/xterm | Yes (package 0.1.39) |

**Gesture:** modified left-click on a hit cell. Linux: **Ctrl+click**. macOS:
**Cmd+click**. Plain left-click stays selection. Shift+click stays host
select (ADR-0003).

**Hit wins the gesture:** if the cell is in a detected or OSC 8 URL, the host
opens it and does **not** start selection and does **not** send an app-mouse
report. If it is not a URL, existing ADR-0001 / ADR-0003 rules apply. One
gesture, one owner.

**Schemes:** allow `http` and `https` only in the first slice. Reject
`javascript:`, `file:`, `data:`. OSC 8 follow-up keeps the same allowlist
until a later ticket.

**Open:** `xdg-open` on Linux, `open` on macOS. Argv only; never `sh -c`.
Spawn detached. Failures: ring BEL, do not crash.

**Hover:** optional underline on the hit run. Not required to ship click.

## First-slice plan

1. Scan the focused pane’s visible rows (and `view_scroll` window) for
   `https?://` runs. Treat wrapping across the row edge as one URL when
   the next row continues the same token. Trailing SGR-safe punctuation
   (`,.;:!?)`]`) is not part of the URL.
2. Ctrl/Cmd+left-press on a hit cell: open, skip `begin_pointer_selection`.
3. Unit tests: wrap, trailing punctuation, `javascript:` reject, hit vs
   miss vs selection.
4. Dogfood: print the GitHub App install URL in a host pane; Ctrl+click
   opens the browser.

OSC 8 parsing and id matching use the same click path. Underline-on-hover
remains optional follow-up work.

## Acceptance (first slice)

- Ctrl/Cmd+click on a visible `https://` URL in `prismattyc-host` opens the
  system browser.
- Plain drag-select of that same text still copies (F6/F7).
- App-mouse vim (tracking on) still gets plain clicks. Ctrl/Cmd+click on a
  URL is host-owned.
- No `file:` / `javascript:` open.
- No change to TTY mux attach.

## Non-goals

- Claiming OSC 8 in `prismattyc-classic/0.1.x`.
- Clickable links inside TTY `prism-mux-attach` (outer terminal).
- In-host browser / webview.
- Right-click “Copy link”.

## Follow-ups (file after this spike is accepted)

- Done: auto-detect + Ctrl/Cmd+click + `xdg-open`/`open` (PR #204).
- Done: OSC 8 parse, cell attribute, id matching, and the same click path.
- hover underline / status-bar URI.
