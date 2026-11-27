# New Window entry points and the shared daemon

Status: **audit of current source, not shipped behavior.** This note changes
no code. It lists every way a new OS window opens, how each one reaches the
shared `pmuxd`, where they behave differently, and a ranked list of fixes.

Base: `origin/main` `27095125`. References name the file and function, so they
stay useful after line numbers move.

## Summary

There are two kinds of "new window", and they behave differently:

- **In-process windows.** File > New Window, the Dock menu, Cmd+N / Super+N,
  the command palette, the Settings window, and windows reopened after a
  restart or update. All of them call `App::open_window` inside the running
  app process. They share that process's menu bar, Dock icon, config, update
  state and `{stem}.host.pid` registration.
- **Separate-process windows.** The Space chip's "Open in new window",
  `pmux space open NAME --new-window`, `pmux layout apply space NAME
  --new-window`, `pmux attach --all`, and a second bare launch on Linux. Each
  starts another `prismattyc-host` process. That process is not the registered
  host while the first one lives. It has its own menu bar, Quit and update
  state.

Both kinds talk to the same `pmuxd` through the same socket
(`PMUX_SOCKET`, else the default instance), so there is one daemon. What
differs is which process owns the window and what its first pane runs.

The main findings:

1. On Windows, opening a Space with no live sessions in a new window focuses
   the existing window instead, and pmux still reports that it opened a host.
2. The in-app "Open in new window" starts a second app process, while
   Cmd+N stays in the same one. Quit, the menu bar and the shared cache then
   depend on which window has focus.
3. A Cmd+N window is a bare local shell outside every Space. A New Session
   made there belongs to no Space.
4. Each separate-process launch truncates the shared host log that an earlier
   detached host may still be writing.

## How a window reaches the daemon

`App::resumed` runs once per process, before the first window. It starts
`pmuxd` if needed (`daemon_startup::ensure_pmuxd_started`, which runs
`pmux up` and makes sure the default Space exists), initializes login
restore, then reopens restart or update windows, or opens the first window.

`App::open_window(event_loop, config_editor)` builds every window. The parts
that decide daemon behavior:

| Step | What it does |
|---|---|
| `register_host_pid` | Writes `{stem}.host.pid` if no live host holds it. Only the registered process may read or write the shared attach-tabs cache. |
| `startup_attach_targets` | `--attach-session` targets apply to the first window only (and to restart windows). Later windows get none. |
| `startup_window_plan` | First window: reads the cache only if registered, and becomes the cache writer if registered. Later windows: `Bare`, never a cache writer. |
| `attach_layout_path` | Cache writer: the shared `{stem}.attach-tabs.json`. Others: a per-window file named from the process id and window id, or `PMUX_VIEW_PATH` / the restart view when given. |
| boot program | Attach targets: `pmux attach --session-id ID` (a log replica of a daemon pane). Startup Space with no targets: the empty-space placeholder. Otherwise: the configured program, by default `$SHELL -l`, as a **local PTY owned by the app process**, not a daemon session. |

So a window shows daemon sessions only when it opens with attach targets or a
Space view. A bare window's panes are local shells that end when the app
process exits. `local_views` saves only their layout and working directories.

An unregistered process takes over registration once the registered host
exits (`App::retry_register_host_pid`, at most once a second), and then
one of its windows becomes the cache writer.

## Inventory

### In-process entry points

All of these end in `UserAction::NewWindow` (or `OpenConfig`), handled in
`App::user_event`, which calls `open_window(event_loop, false)` (or `true`).

| Entry point | Path | Platforms |
|---|---|---|
| File > New Window | `macos_menu::MENU_HOST_ACTIONS` entry `Action::NewWindow` → `UserAction::RunAction` → `UserAction::NewWindow` | macOS |
| Dock menu > New Window | `application_dock_menu_imp` item → `NewWindowTarget::new_window` (`newWindow:`) → `UserAction::NewWindow` | macOS |
| Keybinding `new_window` | default `super+n` (Cmd+N on macOS) → `ActionRoute::OpenWindow` → `Dispatch::OpenWindow` → `UserAction::NewWindow` | all |
| Command palette "new_window" | palette entry → `dispatch_action` → `Dispatch::OpenWindow` → same | all |
| Settings… / `open_config` | `UserAction::OpenConfig`: a terminal editor opens in `open_window(true)` (bare, no cache, one pane); a GUI editor is spawned on its own with no host window | all |
| Restart and update reopen | `restart::resume` and `update_flow::open_saved_windows` call `restart::reopen`, which sets `restart_view` and the saved attach targets for each saved window, then calls `open_window` | all |
| Dock icon click | `application_reopen_imp` only un-minimizes a minimized window. It never opens a new one. The process exits when its last window closes, so a later Dock click is a fresh launch. | macOS |

### Separate-process entry points

| Entry point | Path | Result |
|---|---|---|
| Space chip menu "Open in new window" | `SpaceContextAction::Open(SpaceOpenMode::NewWindow)` → `advance_space_opens` runs `pmux space open NAME --no-run --new-window` → `finish_space_open` → `open_applied_space` | New detached host with `PMUX_SPACE`, a fresh `PMUX_VIEW_PATH` and one `--attach-session` per live session of that Space. The calling window records "new window requested" and gets no view confirmation. |
| `pmux space open NAME --new-window`, `pmux layout apply space NAME --new-window` | same `finish_space_open` → `open_applied_space` | Same as above. Skips the shared cache while another host is live. Refused with `--no-attach`, `--tty` or `--add`. |
| `pmux space open NAME` with no live host | `finish_space_open` → `open_applied_space` | Detached host, which becomes the registered host. |
| `pmux attach --all` | `cmd_attach_all` → `attach_all_command(paths, None)` → `exec` | Replaces the calling CLI process with a host. Attaches **every** live session from **every** Space, with no `PMUX_SPACE` and no view file. It does not check for a live host. |
| Second bare launch | `instance::claim_or_focus` | Windows: focuses the existing window and exits. macOS: the system activates the running app, unless the binary is started directly. Linux: always a second, unregistered process. |

On Linux with no `WAYLAND_DISPLAY` or `DISPLAY`, these paths open no window.
`space open` restores the sessions and says how to attach them. `attach --all`
fails with the same advice (`linux_attach_all_blocked_without_display`).

## Where behavior differs

| | Cmd+N / menu / Dock / palette | Chip "Open in new window", `space open --new-window` | `pmux attach --all` |
|---|---|---|---|
| Process | same app process | new app process | new app process (replaces the CLI) |
| First pane | local `$SHELL -l` | log replicas of the Space's sessions | log replicas of all sessions |
| Space label | none | the Space | none |
| New Session made there | standalone, no Space owner | owned by the Space | standalone |
| Shared cache | never | not while another host is registered | not while another host is registered |
| Takes over registration when the first host quits | no (same process) | yes | yes |
| Quit (Cmd+Q) closes | every in-process window | only this process's windows | only this process's windows |
| Menu bar and update checks | shared with the first window | its own | its own |
| `--panes N` / config `panes` | applies to every new window | not used (attach targets) | not used |

Two windows may show the same Space (documented in `docs/mux-cli.md`). Each
separate-process window keeps its own view file, so this does not mirror the
first window. In current source a Cmd+N window does not mirror either: it
starts bare.

## Findings

Ranked by user impact. Each was confirmed by reading the source at the base
commit. None was reproduced at runtime in this change; see the proof plan.

1. **Windows: a new window for an empty Space focuses the old window.**
   `attach_all_command` allows zero sessions when a Space is named. The new
   host then has no `--attach-session` and no explicit program, so
   `instance::claims_single_instance` returns true. With a bare-launched host
   already running, `claim_or_focus` focuses that window and the new process
   exits. `pmux` has already printed `opened host pid N with 0 session(s)`.
   This affects the chip menu, which offers "Open in new window" for every
   Space, and the CLI. `docs/platforms/windows.md` says
   `space open --new-window` "still open[s] another window".
2. **The in-app "Open in new window" starts a second app process.** The same
   menu that switches Spaces in-process opens a new window out of process.
   Menu, config reload and update-check state are per process. Cmd+Q in the
   new window leaves the old one open, and Cmd+N from it opens a window in the
   second process. Update and restart must handle several processes (the
   update-all proof under `docs/proof/update-all-host-windows` already shows
   two app processes on one daemon).
3. **A Cmd+N window sits outside every Space.** It starts a local shell with
   no Space label, and New Session there makes a session that no Space owns.
   A user in Space "work" who presses Cmd+N gets work that `space save` for
   "work" will not include.
4. **The detached host log is truncated by every detached launch.**
   `open_applied_space` creates `prismattyc-host.log` beside the socket with
   `File::create`. A second detached host truncates it while the first may
   still be writing, so the first host's earlier lines are lost.
5. **`pmux attach --all` ignores Spaces and a live host.** It attaches every
   session from every Space into one unlabeled window, and always starts a
   new process, even when a registered host could show them.
6. **`panes` repeats on every Cmd+N window.** `open_window` uses
   `self.cli.panes` for later windows too. The config reload message says
   `panes` "applies at startup only".
7. **Stale comment (nit).** The `UserAction::NewWindow` comment says only the
   macOS menu builds the variant and that other platforms call `open_window`
   directly. Every platform builds it through `Dispatch::OpenWindow`.

## Recommended fixes

In order. Each is a separate small PR with a test that fails before the fix.

1. **Windows empty-Space window** (finding 1). Treat a launch that carries
   `PMUX_VIEW_PATH` or `PMUX_SPACE` as an explicit window, so it never claims
   the single instance. Test `claims_single_instance` with a view and no
   targets. Leaf, Windows proof batched before release.
2. **Per-process host log** (finding 4). Name the detached log with the child
   process id, or open it for append. Test that a second launch keeps the
   first file's contents. Leaf.
3. **Open Space windows in-process** (finding 2). When the caller is the app
   itself, "Open in new window" seeds a per-window view and calls
   `open_window` with that view and the Space's attach targets, instead of
   spawning `pmux space open --new-window`. The restart path already opens a
   window from a view file and targets (`restart::reopen`), so this reuses
   it. Trunk.
4. **Route CLI new windows to the live host** (findings 2 and 5). When a
   registered host is live, `space open --new-window` and `attach --all`
   send it an open-window request instead of starting a process. The
   per-process request files in `prismattyc_mux::host_tabs` are a working
   pattern for this. Spawn a process only when no host is live. Trunk.
5. **Give Cmd+N a Space** (finding 3). Open the new window as an empty view
   of the focused window's Space, so New Session there is owned by it, or
   keep it bare and show that it is outside every Space. This is a product
   choice; see the open questions.
6. **`panes` on later windows** (finding 6). Either apply it to the first
   window only, or change the reload message. Leaf.
7. **Fix the stale comment** (finding 7) in the same PR as any of the above.

## Proof plan

These checks turn the source reading into runtime proof. They run on a clean
VM with a private `PMUX_SOCKET`, never against a development machine's live
daemon.

1. Launch the app. Record the `pmuxd` process id and the `{stem}.host.pid`
   contents.
2. Press Cmd+N. Expect one app process, two windows, the same `pmuxd` pid,
   and a per-window view file. Expect `pmux ls` to show no new session.
3. Use the chip menu "Open in new window" on a Space with sessions. Expect a
   second app process, the same `pmuxd` pid, and `{stem}.host.pid` still
   naming the first process.
4. Quit the first process. Within a few seconds, expect `{stem}.host.pid` to
   name the second.
5. On Windows, with a bare-launched app running, create an empty Space and
   run `pmux space open EMPTY --new-window`. Today: expect no new window
   (finding 1). After fix 1: a second window.
6. Run two detached opens in a row and compare the log before and after the
   second (finding 4).

## Open questions

1. Should a Cmd+N window join the focused window's Space, or stay a bare
   local window?
2. Should `pmux attach --all` keep showing sessions from every Space, now
   that Spaces own sessions?
3. Once fixes 3 and 4 land, should a second app process for the same daemon
   still be possible, or should it hand its request to the live host and
   exit?
