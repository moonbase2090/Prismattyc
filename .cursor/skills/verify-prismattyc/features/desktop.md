# Desktop UI

The host presents terminals, space navigation, and naming overlays. Native input and the resulting presented frame are part of the behavior.

## Sub-features

- `rail-delete`: confirmation removes a saved space without losing live panes.
- `session-name`: naming and rename overlays preserve visible labels and derive mailbox addresses.
- `host-restart`: terminal state survives a host restart and is presented again.

## How to get to it (user POV)

Open the host, use the space rail context menu to remove a saved space, use the pane/session Rename action, or use the component restart action. The command palette lists action bindings; inspect this build's `prismattyc-host --list-bindings` rather than assuming customized shortcuts.

## Driving it with native fixtures

Preconditions: same-revision binaries from Launch. macOS recipes require a logged-in desktop; rail deletion also requires Accessibility permission for the runner. Linux real-window tests require Xvfb and the dependencies in `scripts/install-render-test-deps.sh`.

- On macOS, run `python3 tests/native/macos-space-delete-e2e.py --bins "$verify_bins" --out "$verify_evidence/rail-delete" --repeat 1`. It sends native keys only to its host PID, opens the real context menu, confirms deletion, and checks a later successful frame. It covers active, live inactive, last, and post-restart deletion under both `tiles` and `iosurface`. Inspect each `result.json`, `host.log`, and `present.png`.
- On macOS, run `python3 tests/native/macos-restart-e2e.py --bins "$verify_bins" --out "$verify_evidence/restart"`. Read its `result.json`, command records, and before/after PNGs. This checks presented pixels and retained live PTYs, not just process restart.
- For Linux naming regression checks, run `cargo +1.90.0 test --locked -p prismattyc-host session_prompt::tests::real_window_session_naming -- --exact --nocapture` after building the binaries. Require one executed test. For a visible naming change, additionally drive the real Rename action, capture the overlay with the typed mixed-case/spaced label, accept it, and capture the resulting row. Check `pmux whoami` from that pane or a read-only snapshot for its derived mailbox. The fixture's internal dispatch alone is not native input proof.

## Gotchas

Keep fixture environments separate from the running desktop. Read `host_pid`, expected space, and a successful frame in `render-status` before injecting events. The present PNG is the host framebuffer, not an OS-level screenshot. Mac presenters, X11, native Wayland, and Windows are distinct proof targets. Existing scripts under `tests/native/docker/` cover Linux native paths; follow `docs/testing-policy.md` for their serialized execution and compositor limits.
