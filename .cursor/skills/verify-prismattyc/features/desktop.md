# Desktop UI

The host presents terminals, space navigation, and naming overlays. Native input and the resulting presented frame are part of the behavior.

## Sub-features

- `rail-delete`: confirmation removes a saved space without losing live panes.
- `session-name`: naming and rename overlays preserve visible labels and derive mailbox addresses.
- `host-restart`: terminal state survives a host restart and is presented again.
- `complete-update`: session-stop consent, private configuration backup, full component restart, and an explicit restoration choice.

## How to get to it (user POV)

Open the host, use the space rail context menu to remove a saved space, use the pane/session Rename action, or use the component restart action. The command palette lists action bindings; inspect this build's `prismattyc-host --list-bindings` rather than assuming customized shortcuts.

## Driving it with native fixtures

Preconditions: same-revision binaries from Launch. macOS recipes require a logged-in desktop; rail deletion also requires Accessibility permission for the runner. Linux real-window tests require Xvfb and the dependencies in `scripts/install-render-test-deps.sh`.

- On macOS, run `python3 tests/native/macos-space-delete-e2e.py --bins "$verify_bins" --out "$verify_evidence/rail-delete" --repeat 1`. It sends native keys only to its host PID, opens the real context menu, confirms deletion, and checks a later successful frame. It covers active, live inactive, last, and post-restart deletion under both `tiles` and `iosurface`. Inspect each `result.json`, `host.log`, and `present.png`.
- On macOS, run `python3 tests/native/macos-restart-e2e.py --bins "$verify_bins" --out "$verify_evidence/restart"`. Read its `result.json`, command records, and before/after PNGs. This checks presented pixels and retained live PTYs, not just process restart.
- For Linux naming regression checks, run `cargo +1.90.0 test --locked -p prismattyc-host session_prompt::tests::real_window_session_naming -- --exact --nocapture` after building the binaries. Require one executed test. For a visible naming change, additionally drive the real Rename action, capture the overlay with the typed mixed-case/spaced label, accept it, and capture the resulting row. Check `pmux whoami` from that pane or a read-only snapshot for its derived mailbox. The fixture's internal dispatch alone is not native input proof.

## Complete update and restoration

`tests/native/full-update-restart-e2e.py` uses private runtime, configuration,
and data paths. It records commands, before/after snapshots, a result file,
and terminal output. Each output directory must be new.

- Run `python3 tests/native/full-update-restart-e2e.py --bins "$verify_bins" --out "$verify_evidence/update-cli"` for daemon replacement, restored directories and commands, retry behavior, and mailbox durability.
- On a logged-in macOS desktop with Accessibility permission, add `--native` with a separate output directory. The fixture sends native keys to its owned host PID, checks cancellation preserves sessions, chooses Save and continue, waits for the replacement app's restore offer, and restores sessions. Inspect the consent, offer, and result PNGs and JSON snapshots. Match each capture to the host instance, PID, and capture sequence in its frame sidecar; sequence numbers reset on exec. The PNGs show presented app pixels, not OS composition.
- In a disposable clean macOS VM only, install the released 0.3.38 app and run the CLI fixture with `--upgrade-app /Applications/Prismattyc.app`. This performs a real release download, verification, and app replacement. It then restarts the private daemon from the installed app and restores the saved configuration. Retain the initial app checksum and actual updater output. This command changes that VM's app installation.

The native fixture's current-release restart path and the real installer
path are separate drives. Record which ran. Neither one substitutes for
clean-VM UI proof. A guest with no active display can supply CLI evidence
only; record the failed capture command and missing display prerequisite.
A Windows desktop is required to prove the Windows replacement-host path.

## Gotchas

Keep fixture environments separate from the running desktop. Read `host_pid`, expected space, and a successful frame in `render-status` before injecting events. The present PNG is the host framebuffer, not an OS-level screenshot. Mac presenters, X11, native Wayland, and Windows are distinct proof targets. Existing scripts under `tests/native/docker/` cover Linux native paths; follow `docs/testing-policy.md` for their serialized execution and compositor limits.
