# Level-0 walkthrough spike

**Status:** Proposed design. This document does not implement the walkthrough.
**Parent:** PT-81, “Level-0 walkthrough.”
**Owner task:** PT-82.
**Workspace baseline:** `0.1.76` for this PR.

This spike defines the first walkthrough slice for `prismattyc-host`. It covers
splash entry, captions, action detection, level data, progress, runtime
placement, accessibility, and offline narration.

## 1. Splash entry and walkthrough flow

Add `Walkthrough` as the fourth splash topic. Keep the existing three topics
unchanged.

| Surface | Entry | Result |
|---|---|---|
| Splash | Press `4` on the main page | Open level 0 immediately |
| Splash | Select `4. Walkthrough` | Open level 0 immediately |
| Splash with progress | Select `4. Resume walkthrough` | Open the first incomplete step |
| Palette | Run the `walkthrough` action | Open the current step |
| CLI | Run `pmux tutorial --play` | Start the same level data in a terminal flow |

The splash remains usable while the walkthrough is selected. Do not make the
walkthrough modal. Do not block the pane while the user performs a step.

Use stable level and step IDs. Do not use display order as an ID. Start with
these levels:

1. **The window:** split right or down, focus with Alt+arrow keys, and close a
   pane.
2. **Tabs:** create, rename, switch, move a pane between tabs, and drag a tab.
3. **Seats:** run `pmux new`, attach with `--all`, use the doorbell, and send,
   claim, and commit mail.
4. **Spaces:** arrange, save, open, and finish a session-ended placeholder.
5. **Power:** use the palette, presets, zoom, find, and synchronized input.

End with a boss step. Ask the user to reproduce a three-seat space from a
reference image. Keep the boss step outside the first implementation slice if
its detector is not ready.

Each step supports these actions:

- **Do it:** show the user the key or command, then wait for the real action.
- **Show me:** run the supported action through the normal dispatch path, then
  wait for the detector event. If the detector observes the successful result,
  complete the step exactly as if the user performed it. A command return code
  alone does not complete the step.
- **Skip:** mark the step skipped and continue.

The walkthrough must not treat its own `Show me` button press as completion.

## 2. Subtitle-style caption overlay

Render the caption as a translucent overlay near the bottom of the host window.
Center it over the pane area like a TV subtitle. Do not dock it under the tab
strip. This keeps it independent of the PT-78 handle row and the PT-80
scrollbar gutter. The overlay must not change pane geometry or PTY dimensions.

Use the existing theme tokens. Alpha-blend the background over the panes. Paint
text with a shadow or outline so it stays readable over terminal content. Keep a
bottom margin of at least one cell plus the configured window padding so the
prompt stays above the window edge.

Show at most two lines:

```text
┌──────────────────────────────────────────────┐
│ Split the pane to the right.              ×   │
│ Ctrl+Alt+Enter    [show me] [skip]            │
└──────────────────────────────────────────────┘
```

Keep the imperative caption on line one. Put the key or command and the
optional controls on line two. Wrap only at word boundaries. Truncate long
hints with an ellipsis instead of creating a third line. Resolve the displayed
chord from the active keymap. Do not hard-code a chord that can become stale
after a config change.

Draw an `×` dismiss control for every caption. Only the `×` hit target consumes
pointer input. All other pointer and keyboard input passes to the pane. Dismiss
hides the current caption but does not skip or complete the step; keep the
detector armed and show the next caption after the current step resolves. Do
not make the overlay modal.

Keep the caption visible until the detector observes success, the user skips
the step, the user dismisses it, or the user exits the walkthrough. When a step
succeeds, show a short confirmation in the same overlay. Then advance to the
next step. Keep the confirmation non-blocking and time-limited.

Use the theme foreground and background tokens. Select a foreground with clear
contrast in both dark and light themes. Test the overlay with every bundled
theme. Do not rely on color alone to identify the expected action. Keep the
sentence and command text visible when accent colors are unavailable.

Respect reduced-motion settings. Do not animate the overlay when reduced motion
is enabled. Show the same static overlay and keep all detector behavior active.

Captions are the source of truth. Store caption text in the level data. Use the
same text for narration generation. Do not maintain a separate narration
script.

## 3. Action detection

Detect real results. Do not detect a key press or the walkthrough's own button
press as task completion.

The host action names are the `Action` variants in
`crates/prismattyc-host/src/keybind.rs:26-72`. Their stable `[keys]` names and
descriptions are defined in `crates/prismattyc-host/src/keybind.rs:74-215`.
The host receives and dispatches those actions in
`crates/prismattyc-host/src/main.rs:5149-5193`.

Use one normalized event vocabulary for all producers:

```text
host_action(action, result)
mux_event(kind, ids)
space_event(kind, path)
command_event(command, result)
```

The first detector adapters are:

| Producer | Source and live boundary | Examples | Detector rule |
|---|---|---|---|
| Host action dispatch | `keybind.rs:26-72`; `main.rs:5095-5139` | `split_right`, `focus_right`, `new_tab`, `rename_tab`, `move_pane_next_tab`, `move_tab_right`, `preset_grid`, `zoom_pane`, `find` | Emit `host_action` after the action returns success. |
| `pmuxd` control events | Variants in `crates/prismattyc-mux/src/control.rs:1641-1767`; shared emitter `control.rs:4795-4805` | `PaneSplit`, `WindowCreated`, `WindowRenamed`, `PaneMoved`, `MailAttentionChanged`, `SyncInputChanged` | Match the event kind and the expected IDs or state. |
| Space operations | `docs/mux-cli.md:210-223` describes the host cache and space-file boundary | save and open | Emit `space_event` after the file operation succeeds. |
| CLI commands | `docs/mux-cli.md:45-70` documents the `pmux` command boundary | `pmux new`, attach, and mail commands | Emit `command_event` after the command completes. |

The current code has no walkthrough event bus. This is a required follow-up,
not an assumption in this spike. Add the host emission at the successful return
boundary in `main.rs:5095-5139`. Add a control-client adapter around the
existing event stream. Add space and command adapters at their successful
operation boundaries. Do not infer an action from a repaint, key repeat, or
screen text.

A step declares the event kind and an optional predicate. The predicate may
match the focused pane, window, session, target path, or resulting value. Keep
predicates small and deterministic.

Examples:

```toml
expect = { kind = "host_action", action = "split_right", result = "ok" }
expect = { kind = "mux_event", event = "PaneMoved", to_window = "current" }
expect = { kind = "space_event", event = "saved" }
```

A failed action does not complete a step. Report the failure in the caption
band and continue waiting. A timeout shows a hint; it does not invoke the
command or alter progress.

## 4. Level data schema

Keep levels in one file. Use TOML for authoring and review. A future loader can
validate the file before the host starts.

Use a schema like this:

```toml
schema_version = 1

[[level]]
id = "window"
title = "The window"

[[level.step]]
id = "window.split-right"
caption = "Split the pane to the right."
hint = "Use the key shown in the caption."
command = "split_right"
expect = { kind = "host_action", action = "split_right", result = "ok" }
show_me = { kind = "host_action", action = "split_right" }
```

Required fields are `id`, `title` on a level, and `id`, `caption`, `expect` on
a step. Optional fields are `hint`, `command`, `show_me`, and `audio`.

Validate these rules:

- IDs are unique and use lowercase ASCII with `-` separators.
- A level contains at least one step.
- A step has one expectation.
- A `show_me` action has a matching detector.
- Captions are short enough for the caption band.
- An unknown event kind fails validation before runtime.

Draft these concrete rows for levels 0–4 and the boss. They are the initial
content contract, not a claim that all detectors already exist.

| Level | Step ID | Caption | Expectation |
|---|---|---|---|
| 0 The window | `window.split-right` | Split the pane to the right. | `host_action(split_right, ok)` |
| 0 The window | `window.focus-right` | Focus the pane on the right. | `host_action(focus_right, ok)` |
| 0 The window | `window.close-pane` | Close the focused pane. | `host_action(close_pane, ok)` |
| 1 Tabs | `tabs.new` | Open a new tab. | `host_action(new_tab, ok)` |
| 1 Tabs | `tabs.rename` | Rename the active tab. | `host_action(rename_tab, ok)` |
| 1 Tabs | `tabs.move-pane` | Move a pane to the next tab. | `host_action(move_pane_next_tab, ok)` |
| 1 Tabs | `tabs.drag` | Drag the tab one slot to the right. | `host_action(move_tab_right, ok)` |
| 2 Seats | `seats.new` | Create a new pmux seat. | `command_event(pmux_new, ok)` |
| 2 Seats | `seats.attach-all` | Attach all seats. | `command_event(attach_all, ok)` |
| 2 Seats | `seats.mail` | Send and commit a letter. | `command_event(mail_commit, ok)` |
| 3 Spaces | `spaces.save` | Save this space. | `space_event(saved)` |
| 3 Spaces | `spaces.open` | Open the saved space. | `space_event(opened)` |
| 3 Spaces | `spaces.reopen` | Reopen the ended session. | `mux_event(SessionCreated, current)` |
| 4 Power | `power.palette` | Open the command palette. | `host_action(command_palette, ok)` |
| 4 Power | `power.preset` | Arrange the panes as a grid. | `host_action(preset_grid, ok)` |
| 4 Power | `power.zoom` | Zoom the focused pane. | `host_action(zoom_pane, ok)` |
| 4 Power | `power.find` | Find text in scrollback. | `host_action(find, ok)` |
| Boss | `boss.reproduce-space` | Reproduce the three-seat picture. | `space_event(boss_snapshot_match)` |

Use TOML tables for the concrete rows. The table above is the review contract
for those entries. Keep the boss row outside the first implementation slice
until its snapshot matcher exists.

Place the first implementation file at
`crates/prismattyc-host/walkthrough/levels.toml`. Keep the file independent of
rendering code so `pmux tutorial --play` can consume the same content later.

## 5. Progress schema

Persist progress per user at `$XDG_DATA_HOME/prismattyc/walkthrough.json`. If
`XDG_DATA_HOME` is unset, use `~/.local/share/prismattyc/walkthrough.json`.
Do not use the repository root, an absolute `/prismattyc` path, or a session
directory.

Use an atomic replace. Write a temporary file in the same directory, flush it,
and rename it over the old file. If the file is missing or invalid, start at
level 0 and keep the invalid file for diagnostics or replace it after the first
successful update.

Proposed schema:

```json
{
  "schema_version": 1,
  "current_level": "window",
  "current_step": "window.split-right",
  "completed": ["window.split-right"],
  "skipped": [],
  "updated_at": "2026-08-29T00:00:00Z"
}
```

Use step IDs in `completed` and `skipped`. Do not store pane IDs, process IDs,
window geometry, or host-specific paths. These values do not survive a new
session.

Persist after detector-confirmed success or skip. For `Show me`, persist only
after the normal action dispatch returns success and the matching detector
event arrives. Resume at the first step that is neither completed nor skipped.
Replaying a completed step does not remove its record. Provide a reset action
later; the first slice only needs resume and skip.

## 6. Runtime placement and ownership

Make `prismattyc-host` the primary walkthrough client. It owns the splash,
caption band, keymap display, host-action detectors, and audio playback.

Keep the level file and progress rules reusable. Add `pmux tutorial --play`
through a terminal client after the host slice proves the schema. The terminal
flow may render captions as text, but it must use the same step IDs and
expectations.

Do not put the primary walkthrough UI in `pmux-attach`. `pmux-attach` paints a
session into an outer terminal and does not own the windowed splash. It may
consume the shared level data in a later slice if a terminal-only tutorial is
needed.

Use existing ownership boundaries:

- Host actions emit local events after successful dispatch.
- `pmuxd` remains the authority for sessions, windows, panes, and mail state.
- The control client observes `pmuxd` events; it does not mutate state to make
  a step pass.
- Space code emits save and open results after the operation succeeds.
- The walkthrough state machine consumes normalized events and updates only
  walkthrough progress.

This keeps walkthrough state separate from terminal state. It also prevents a
walkthrough from bypassing the normal input, lease, or control paths.

## 7. Offline narration, accessibility, and asset budget

Generate narration clips offline. Add a script at
`scripts/walkthrough-voice.sh` in the implementation ticket. The script reads
the caption and step ID from the level file and calls the ElevenLabs API only
when the operator runs the script.

Read the API key from the environment. Never store the key in the repository.
Use the ElevenLabs voice **Russ** by name. Default the generation script to
`Russ`. Resolve the current voice ID through the ElevenLabs voices endpoint
when the script runs. Never hard-code that ID. Use one voice for the complete
clip set and record the voice name, resolved voice ID, and model in the asset
manifest.

Never call ElevenLabs at runtime. Fail the script when a requested clip cannot
be generated; do not create a partial release set without reporting it.

For the first slice, budget 16 clips: one intro plus 15 level-step clips. Keep
each clip at or below 100 KiB. The arithmetic is `16 × 100 KiB = 1.6 MiB`,
leaving 0.4 MiB below the 2 MiB bundle target for metadata and filesystem
rounding. Measure a representative generated clip before committing assets.
The asset check must sum the actual files and fail above 2 MiB. This spike has
no generated clip, so it records the measurement rule rather than inventing a
sample size.

Store small OGG or MP3 clips in the host bundle. Name each clip with its step
ID, for example `window.split-right.ogg`.

Play a bundled clip through the existing host bell-sound path. Reuse its
process selection, failure handling, and one-second minimum gap. Add these
future config keys:

```toml
walkthrough_audio = true
walkthrough_voice = "russ"
```

Write an asset manifest next to the clips. Record the stable step ID, clip
path, voice name (`Russ`), resolved voice ID, and ElevenLabs model for every
clip. The manifest makes a release reproducible without putting a provider ID
in the level file.

```json
{
  "voice": "Russ",
  "model": "eleven_multilingual_v2",
  "clips": [
    {
      "step_id": "window.split-right",
      "path": "window.split-right.ogg",
      "voice_id": "resolved-at-generation"
    }
  ]
}
```

### Accessibility

Keep captions enabled by default. Audio is never the only channel. A missing
clip, disabled audio, or unavailable audio player must leave the caption flow
fully usable. Keep the caption text selectable by a screen reader in the host
accessibility work planned by PT-34. Do not encode required instructions only
in color, sound, or animation.

Enable audio by default only when the requested clip is bundled. Provide a user
setting to turn audio off without disabling the walkthrough.

## Ordered follow-up ticket titles

Create these tickets in order after the spike is accepted:

1. **Implement walkthrough level schema and validation.**
2. **Add splash topic 4 and the non-modal subtitle caption overlay.**
3. **Add host-action and pmuxd event detector adapters.**
4. **Add XDG progress persistence, resume, skip, and reset.**
5. **Add `pmux tutorial --play` using the shared level data.**
6. **Add offline ElevenLabs generation and bundled audio playback.**
7. **Add the boss snapshot matcher and end-to-end walkthrough fixtures.**

## Decision summary

- Put the first walkthrough UI in `prismattyc-host`.
- Keep captions in one level file and make them authoritative.
- Detect completed actions from real host, `pmuxd`, space, and command results.
- Treat `Show me` as complete only after normal dispatch and detector success.
- Persist only stable step IDs and resume metadata under the XDG data directory.
- Generate narration offline. Make runtime playback local and optional.
- Reuse the level schema for a later `pmux tutorial --play` flow.

## Out of scope for this spike

- Implementing splash topic `4` or the caption renderer.
- Adding the level loader or progress writer.
- Adding ElevenLabs credentials, generated audio, or network code.
- Defining the complete boss-level fixture.
- Changing `pmux-attach` behavior.
