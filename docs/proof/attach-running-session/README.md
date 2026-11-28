# Attach running session: user-flow proof

`tests/native/attach-session-e2e.py` drives a real app against a private
pmux server, with Space `work` (session `work-1`), Space `release` (session
`rel-1`, plus `build` added to it) and a session `loose` in no Space. Frames
are the app's own presented frames. `*-result.json` lists the checks.

| Platform | Where | Result |
| --- | --- | --- |
| macOS 26.6.2 arm64 | fresh Tart VM from the base image, built in the VM at 9163d1f0, deleted after | PASS in 3 of 4 runs. The first, right after the build, timed out on "focus existing"; the test now prints the picker and focus state if that happens again. |
| Linux | Debian bookworm container with Xvfb and openbox, built in the container | PASS |
| Windows | batched before release | — |

Checks, in order:

1. Open: the picker lists `loose` under Not in a Space, `build` under In other Spaces (Move here) and `work-1` under Already open.
2. Pick: Enter on `loose` adds it to `work` and opens it in a new, focused tab.
3. Focus existing: with tab 1 focused, picking `loose` (Already open, Focus) focuses its tab and opens no new one.
4. Detach: Ctrl+Shift+X closes the tab; `loose` keeps running and stays in `work`.
5. Reopen: `loose` is listed under In this Space; Enter opens it again.
6. Move: Enter on `build` first shows "build leaves release and joins work · Enter to move · Esc" and moves nothing; Enter again moves it to `work` and focuses it.
7. Escape closes the picker.

The run records `space_save_status_after_detach` without asserting it.
Detaching a Space-owned session makes the next autosave refuse to drop it;
that predates this change and is tracked separately.
