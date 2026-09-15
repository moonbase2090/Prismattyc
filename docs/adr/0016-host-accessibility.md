# ADR-0016 — Host accessibility (OS tree + announce)

- **Status:** Accepted (2026-09-01; Linux AT-SPI probe retried the same day)
- **Ticket:** PT-34
- **Depends on:** ADR-0006, ADR-0010, ADR-0012
- Plain grid text remains available. Window controls also have an OS accessibility tree.

## Clean-room process (binding)

This decision follows the same rule as ADR-0001:

1. Study public accessibility behaviour and APIs.
2. Write Prismattyc policy in this file.
3. Implement original code from this ADR and existing host state.
4. Do not copy another terminal emulator’s accessibility sources.
5. Do not vendor GPL accessibility implementations.

**Studied for practice (ideas only — no code incorporated):**

| Kind | What we used it for |
|---|---|
| AccessKit and `accesskit_winit` public docs | Cross-platform tree → NSAccessibility / AT-SPI / UIA |
| AT-SPI and NSAccessibility role names | Window, tab list, document, live region |
| Common terminal VoiceOver behaviour (as products) | One text surface + chrome, not a cell grid of nodes |
| This-machine probe 2026-09-01 (retry) | Host PID absent from AT-SPI; Plasma taskbar is a childless `Prismattyc` button |

## Context

`prismattyc-host` is a daily-driver OS window. Screen-reader users must
operate it. macOS VoiceOver is the named target. Linux Orca is in scope.

The host paints a CPU pixel buffer. Tabs, panes, palette, splash, and the
cell grid are not toolkit widgets. A live AT-SPI dump on this Linux seat
(with `org.a11y.Status.IsEnabled=true`) found no application name for
any `prismattyc-host` PID. Plasma’s taskbar exposes one childless
`push button` named `Prismattyc`. Empty pane, prompt, and Kiro banner
are the same silence. VoiceOver on a raw winit `NSView` has the same
gap: window title only.

Plain grid text remains the guaranteed textual surface. You can use the
keyboard to reach window controls. Walkthrough captions must remain
readable without audio.

## Decision

### D-A1 — Both an OS tree and a product announce channel

| Channel | Owner | Job |
|---|---|---|
| OS accessibility tree | AccessKit via `accesskit_winit` | Roles, names, values, focus, actions |
| Product announce | Host live-region + optional bell cue | Speak *changes* (mail, attention, cursor line, selection) |

Do not ship announce-only. Do not ship a tree that omits chrome. Audio is
never the only channel.

### D-A2 — AccessKit is the adapter, not a vendored terminal tree

Add `accesskit` and `accesskit_winit` to `prismattyc-host`. Keep winit 0.30.
The adapter maps one tree to:

- macOS: NSAccessibility (VoiceOver)
- Linux: AT-SPI (Orca)
- Windows: UI Automation (later; not a PT-34 gate)

Build the tree from mux and host chrome state. Do not scrape pixels. Do
not import another emulator’s accessibility module.

### D-A3 — Chrome is real nodes

Every interactive host surface is a node with a role, a name, and a
keyboard action that already exists.

| Surface | Role | Name source |
|---|---|---|
| OS window | Window | `window_title` (keep the title in sync) |
| Tab strip | TabList / Tab | Tab title; mail and unseen badges in the description |
| Pane | Pane or Group | Pane title, else session / agent id |
| Command palette, splash, find, theme, space rail | Dialog or complementary | Overlay title already used in the window title |
| Scrollbar | ScrollBar | `N/M` chip text |
| Walkthrough caption (when present) | Static text | Caption lines; not audio-only |

Focus in the tree follows host focus. Activating a tab or palette row
dispatches the existing action. Hover-only paths stay forbidden.

### D-A4 — The focused pane is one document, not a cell grid

Expose the **focused** pane’s **visible** lines as one Document (or
multiline text input) node.

- **Value:** row-major plain text of the viewport. This is A11y v0.
- **Caret:** emulator cursor as a character offset into that value.
- **Selection:** host selection range when present.
- **Scrollback:** not in the live node. Find and `pmux save-buffer`
  remain the history path.
- **Unfocused panes:** name and badge only. Do not stream every pane.

Do not create one node per cell. Do not special-case guest TUI chrome
(Kiro, nvim). Those bytes are grid text. Guest-owned screen-reader speech
stays out of scope.

### D-A5 — Announce changes; rate-limit the grid

Push short live-region strings for:

1. Mail doorbell: agent id and summary when depth rises.
2. Agent attention / permission prompt (`notify::attention` already fires
   an OS notification; announce the same words).
3. Cursor line when the cursor row changes or the pane gains focus.
4. Selection contents when a selection completes or copy succeeds.

Do not announce every PTY byte. Coalesce grid announces. Reuse the host
bell-sound path as an optional cue only.

### D-A6 — Scope

**In scope:** `prismattyc-host` on macOS and Linux.

**Out of scope until a later ADR:**

- `pmux-attach` TTY
- Nested classic `prismattyc`
- Guest TUI speech
- Rich-attachment accessibility tree (A11y v0 still applies)

### D-A7 — Config

```toml
[a11y]
os_tree = true
announce = true
```

Both default on. `os_tree = false` skips AccessKit registration.
`announce = false` keeps the tree and silences live-region speech.

## Consequences

- The adapter exposes tab and overlay navigation to native assistive
  technologies. Verify each target with its screen reader.
- The focused prompt and guest output are exposed as document text.
  Native assistive-technology validation is separate from tree unit tests.
- CI tests the tree snapshot. Operator VoiceOver on macOS is the
  named-target proof. Linux AT-SPI dump is the secondary proof.
- `accesskit_unix` needs a working AT-SPI registry on the seat. This
  probe found a broken registry activation. That is an environment
  defect. It does not change D-A2.

## Follow-up tickets

The implementation contains these three parts:

1. AccessKit adapter and host chrome tree (D-A2, D-A3).
2. Focused-pane grid as one document node (D-A4).
3. Live-region announces for mail, attention, and cursor line (D-A5).

## Implementation evidence

The 2026-09-01 probe above records the pre-implementation environment.
It is not a claim that the current host lacks an accessibility tree.
See [accessibility controls and native checks](../accessibility.md) for
the implemented surface, fixture, and limits of the evidence.
