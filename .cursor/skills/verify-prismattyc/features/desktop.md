# Desktop UI

The host presents terminals, space navigation, and naming overlays. Native input and the resulting presented frame are part of the behavior.

## Sub-features

- `rail-delete`: confirmation removes a saved space without losing live panes.
- `session-name`: naming and rename overlays preserve visible labels and derive mailbox addresses.
- `host-restart`: terminal state survives a host restart and is presented again.
- `window-restore`: a Dock click restores a minimized macOS window, including when another window remains visible.

## How to get to it (user POV)

Open the host, use the space rail context menu to remove a saved space, use the pane/session Rename action, or use the component restart action. To restore a window, open a second window with New Window, minimize it with the yellow titlebar button, and click the app's Dock icon. The command palette lists action bindings; inspect this build's `prismattyc-host --list-bindings` rather than assuming customized shortcuts.

## Driving it with native fixtures

Preconditions: same-revision binaries from Launch. macOS recipes require a logged-in desktop with an active display. Rail deletion and Dock restoration require Accessibility permission for the runner. Dock restoration also requires permission to capture the screen. Linux real-window tests require Xvfb and the dependencies in `scripts/install-render-test-deps.sh`.

- On macOS, run `python3 tests/native/macos-space-delete-e2e.py --bins "$verify_bins" --out "$verify_evidence/rail-delete" --repeat 1`. It sends native keys only to its host PID, opens the real context menu, confirms deletion, and checks a later successful frame. It covers active, live inactive, last, and post-restart deletion under both `tiles` and `iosurface`. Inspect each `result.json`, `host.log`, and `present.png`.
- On macOS, run `python3 tests/native/macos-restart-e2e.py --bins "$verify_bins" --out "$verify_evidence/restart"`. Read its `result.json`, command records, and before/after PNGs. This checks presented pixels and retained live PTYs, not just process restart.
- On macOS, run `python3 tests/native/macos-window-restore-e2e.py --bins "$verify_bins" --out "$verify_evidence/window-restore" --revision "$(git rev-parse HEAD)"`. The fixture rejects a host whose embedded revision differs before sending input. It uses native Cmd+N, the yellow titlebar button, and its own Dock icon. Require `result.json` to report `PASS` for a visible sibling, normal activation, all windows minimized, and the last window. Inspect `native-result.json`, both render-status files, OS window screenshots, and `cleanup.json`. Window count must remain unchanged during restoration. This drive covers the Dock icon; minimized thumbnails and the Window menu need separate native checks when those entry points change.
- For Linux naming regression checks, run `cargo +1.90.0 test --locked -p prismattyc-host session_prompt::tests::real_window_session_naming -- --exact --nocapture` after building the binaries. Require one executed test. For a visible naming change, additionally drive the real Rename action, capture the overlay with the typed mixed-case/spaced label, accept it, and capture the resulting row. Check `pmux whoami` from that pane or a read-only snapshot for its derived mailbox. The fixture's internal dispatch alone is not native input proof.

## Gotchas

Keep fixture environments separate from the running desktop. Read `host_pid`, expected space, and a successful frame in `render-status` before injecting events. The present PNG is the host framebuffer, not an OS-level screenshot. Mac presenters, X11, native Wayland, and Windows are distinct proof targets. Existing scripts under `tests/native/docker/` cover Linux native paths; follow `docs/testing-policy.md` for their serialized execution and compositor limits.

## Complete settings and fonts

The `space_settings` command-palette action and a Space chip's **Spaces
settings…** menu open the existing settings page while `settings_panel` is
absent or false. **Enable full settings (preview)** enables the complete
editor. The new editor can disable or reset its own gate.

Run these non-window probes with the same Rust toolchain and build environment
as Launch:

```sh
PRISMATTYC_DUMP_SETTINGS="$verify_evidence/settings-painter" \
  cargo +1.90.0 test -p prismattyc-host settings::tests --locked -- --test-threads=1
cargo +1.90.0 test -p prismattyc-mux config::tests --lib --locked -- --test-threads=1
python3 scripts/check-settings-mutations.py --revision HEAD \
  --target "$verify_target" --out "$verify_evidence/settings-mutations"
```

The first command traverses actual section/editor rows, enters literal values,
saves and loads every setting, captures every binding, and checks reset,
compound edits, list/remote order, invalid known values, unknown fields,
comments, conflicts, and coalesced reload receipts. The mutation driver creates
an isolated detached worktree, runs a passing baseline, then requires behavioral
assertion failures for a missing control, writer, finite choice, and newly
accepted field without a control. It retains baseline, failure logs, and
`result.json`, then removes its owned worktree. Serialize this command with
other builds using the same Cargo target.

Inspect the classic and Graphite sections/font-size PNGs from the first command.
They are offscreen painter output, with selected-row hit tests and visible-label
assertions. They are not native desktop captures.

After desktop allocation, use Python 3.11 or newer for the dedicated native fixture:

```sh
python3 tests/native/macos-settings-e2e.py --bins "$verify_bins" \
  --out "$verify_evidence/settings-native" --revision "$(git rev-parse HEAD)"
```

The headless fixture-boundary checks can run without a desktop:

```sh
python3 tests/native/macos-settings-e2e_test.py
```

It checks its own host PID/frame, uses native keys for enablement/navigation/text
and Accessibility actions for visible editor commands, captures only owned
windows, checks durable values and unchanged session/PTY content, and compares
both bundled fonts on identical terminal content. Inspect its captures and
`result.json`; an unexecuted fixture is not proof. The read-only
`--check-desktop-only` option checks prerequisites without starting processes.

These probes do not prove desktop pixels or native input routing. Existing
rail-delete and restart fixtures do not cover the settings editor. Native
settings proof needs an isolated owned host and daemon, the doctor above, and
native input for each of these steps under both classic and Graphite chrome:

1. Begin without `settings_panel`, open the existing page, enable the preview,
   and capture the complete editor.
2. Use plain Left/Right on the section row, Down into rows, Up back to the
   section row, and Left/Right to change a choice. Enter a text field and verify
   arrows move its caret. Bind these keys to conflicting terminal actions in
   the fixture and verify the modal consumes them without PTY input.
3. Enter `font_px = 18.5`, save, and inspect both the durable file and a later
   successful frame. Reset and verify the key disappears. Exercise compound
   colors, ordered lists, bindings, and destination changes through visible
   action rows; capture action and resulting state.
4. Compare the default, `bundled:ibm-plex-mono`, and `bundled:commit-mono` using
   identical neutral terminal content, size, theme, and Graphite chrome. Record
   the actual loaded face, environment override precedence, and fallback after
   an unreadable custom path. Keep the default unchanged. Retain package license
   listings and measured binary/package sizes from comparable release builds.
5. Disable or reset `settings_panel`, reopen settings, and verify existing-page
   routing. Separately check live changes after watcher reload and controls
   labeled for the next startup or daemon start.

No host or VM input is permitted without the coordinator's desktop allocation.
A missing allocation, display, Accessibility permission, or relevant OS is an
unmet prerequisite. Report it separately from passing non-window probes.
