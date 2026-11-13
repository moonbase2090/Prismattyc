# Changelog

## [Unreleased]

### Fixed

- A quiet Windows install no longer waits for the installer to time out after starting `pmuxd`. The start script now launches `pmux up` through the shell, so the install finishes when the script does.

## [0.3.65] - 2026-11-13

### Fixed

- The release text check accepts ordinary desktop compositor prose while still rejecting uppercase Windows machine identifiers.
- The Activity Monitor package is now 1.3.2 so its updated sources have a new immutable archive identity.
- The terminal presenter preserves split arrow and Alt sequences, delivers lone Escape without waiting for another key, and keeps an idle bracketed paste open.
- Windows updates restore a missing Activity Monitor command forwarder after verifying the installed frontend.

## [0.3.65-rc.1]

Release candidate for 0.3.65, with the [same changes](#0365---2026-11-13).

## [0.3.64] - 2026-11-12

### Added

- The Activity Monitor can remove one or all stopped sessions with confirmation. Live sessions cannot be removed, and untracked historical rows can be hidden without daemon cleanup.
- `pmux-mcp --activity-only --listen 127.0.0.1:PORT` serves the four read-only activity tools over streamable HTTP, for clients that reach the machine through a tunnel. It is off unless configured, binds loopback only, requires a bearer token from `--token-env` or `--token-file` (the file form is refused on Windows, where its access rights cannot be checked), and refuses to start together with `--as`, `--plugin` or `--supervise`.

### Fixed

- The live sidebar tab row shows a tab's fixed name; unnamed tabs keep their derived label.
- Escape closes every dialog, menu and overlay even when the platform reports it as text or only by its physical key, or while a modifier is held or stuck. The command palette, the Space picker and its delete confirmation, tab and Space rename, and the Space rail previously ignored Escape under Ctrl, Alt or Super.
- The launch splash and the command palette fit small windows. A window too narrow or short for the block wordmark shows a two-row logo, then a one-line wordmark, and the splash copy wraps or is cut with an ellipsis instead of running off the edge. A fractional display scale gives fewer columns and rows for the same pixels, so it now picks the smaller forms where it must. A short window keeps the palette's command list by leaving out its detail panel, a narrow palette drops a row's keybinding when it would squeeze the name, and the palette's footer is cut with an ellipsis.

## [0.3.64-rc.1]

Release candidate for 0.3.64, with the [same changes](#0364---2026-11-12).

## [0.3.63] - 2026-11-11

### Changed

- Attached-pane log replay no longer redraws panes when the visible content is
  unchanged.

### Fixed

- Startup waits for the helper's started marker before reading it.
- Updated rustix to 1.1.6 to address an xattr advisory.

## [0.3.63-rc.1]

Release candidate for 0.3.63, with the [same changes](#0363---2026-11-11).

## [0.3.62] - 2026-11-10

### Fixed

- The activity monitor package is now 1.2.0, so the block and ASCII graph views and the braille default are not hidden behind an older 1.1.0. The daemon moves an enabled default plug-in that sits on an older version than the bundled one to the bundled version at start, keeping its grants; a disabled plug-in stays disabled. `pmux-activity-monitor` falls back to the default view, with a note, when the installed package lacks the requested graph view. CI now refuses plug-in sources that change under a version the pinned bundle already holds, and release branches and tags refuse a source version the pin does not hold.
- This release embeds the official 1.2.0 plug-in catalog, so core builds ship activity monitor 1.2.0 and keep earlier packages for rollback.

## [0.3.62-rc.1]

Release candidate for 0.3.62, with the [same changes](#0362---2026-11-10).

## [0.3.61] - 2026-11-10

### Fixed

- Activity hook observation now shares one two-second deadline across stdin and the activity exchange.
- `pmux mail watch` and connected clients now distinguish a quiet mailbox from a daemon that exited, and return promptly when the daemon stops.
- Escape closes the pane menu and control panel even when a modifier appears held, the menu has no rows, or the platform reports Escape as a control character. The host also clears held modifiers when its window loses focus.

## [0.3.61-rc.1]

Release candidate for 0.3.61, with the [same changes](#0361---2026-11-10).

## [0.3.60] - 2026-11-08

### Changed

- The activity monitor uses braille graphs by default. Settings, `activity_graph_style` and `--graph-style` can select braille, block or ASCII; `TERM=dumb` selects ASCII.

### Fixed

- On Windows, Update and Restart relaunches saved agent commands from the current executable after an updater renames the running executable, instead of leaving agent panes as bare shells.

## [0.3.60-rc.1]

Release candidate for 0.3.60, with the [same changes](#0360---2026-11-08).

## [0.3.59] - 2026-11-07

### Added

- The command palette has an `activity_monitor` entry that opens a Space named Activity Monitor, or switches to it when it already exists. The Space holds one session running the activity monitor. The monitor starts again when it was quit. `pmux space monitor` does the same from a terminal. The entry has no default shortcut; bind one with `activity_monitor` under `[keys]`. `PMUX_ACTIVITY_MONITOR` overrides the monitor path. Opening it on a fresh install installs and enables the two activity plug-ins with the local read grant, and no other grant.
- Plug-in views get mouse input. The plug-in API is now `pmux:plugin@2.0.0` and the view interface `pmux:view@2.0.0`, with a `mouse` event carrying a press, release or wheel turn and its cell. A new event case breaks components built for 1.x, so plug-ins built for 1.x are listed as incompatible until they are rebuilt. Hosts forward presses, releases and the wheel but not pointer motion. In the activity monitor, a click selects a seat, a second click on it opens its details, the wheel moves the selection or scrolls the timeline and details, and a right-click goes back to the list where the terminal passes it through. The desktop app keeps plain right-click for its pane menu; Shift+right-click passes through and returns the monitor to its list, as does Esc. The activity monitor package is now 1.1.0 and the connector 1.0.1, both built for the new interface. An enabled plug-in whose selected package is incompatible moves to the newest bundled version that runs on this core, keeping its grants; older packages stay listed as incompatible, and nothing moves when no compatible version is bundled.

### Changed

- Session labels preserve mixed case and spaces by default. Set `allow_mixed_case_session_names = false` under `[mux]` to use the previous mailbox-address format.

### Fixed

- Blank terminal panes remain visible when switching to a previously inactive Space.
- Removing managed activity hooks restores the original provider settings shape, including absent keys, empty objects and empty arrays.

### Testing

- Release checks cover executable version agreement, sessions, mail, saved Spaces, and Update and Restart with program restoration across the supported platforms.

## [0.3.59-rc.1]

Release candidate for 0.3.59, with the [same changes](#0359---2026-11-07).

## [0.3.58] - 2026-11-05

### Changed

- The activity monitor and activity connector plug-ins are ready with no setup. The first time a daemon built with the official bundle finds either one with no saved state, it installs the bundled version and enables it with local activity access only, so `pmux-activity-monitor` works in a pane on a fresh install. Disabling either one in Settings > Plug-ins or with `pmux plugin disable` keeps it off across restarts and updates. Remote endpoints still need explicit grants, and packages installed by hand still start disabled.
- The activity feed is on by default. pmuxd serves seat activity metadata without any setup, and `experimental_activity = false` under `[mux]`, in the settings panel or the config file, turns it off live. Configs that already contain `experimental_activity = false`, including ones written by earlier versions, keep the feed off. The feed carries no command text, paths, prompts, tool output or mail bodies, and monitor and query access are still granted separately in Settings > Plug-ins.

### Fixed

- Space rename, deletion, undo and pane-title changes run on a worker thread. Uncached daemon snapshots and Space directory reads also stay off the window thread.
- Start fresh saves an empty host view, so restarting the app or daemon does not reopen the previous Space. Saved Space definitions and daemon sessions remain available for explicit reopening.
- The settings panel shows the built-in default for `[mux]` keys that are not in the config file, so the first Enter on a boolean changes it.
- On Windows, opening the activity monitor no longer opens an empty console window. pmuxd now starts its plug-in worker, webhook sender and default-Space `pmux` calls without a console window.
- Restarting pmuxd with a session configuration, as Update and Restart does, no longer empties the Spaces. The restart used to stop each session while Space autosave was still running, so autosave saved every Space without its sessions. The restored sessions then belonged to Spaces whose files no longer listed them. Autosave now stays off for those Spaces from the restart until `pmux session-config restore` has put every session back.
- `pmux stop` no longer reports a failure when pmuxd exits between the check that it is running and the signal to stop it.
- Session restoration preserves pane working directories and original explicit launch arguments for ordinary programs. Known coding agents keep supported conversation selectors and options and resume without replaying startup prompts. Missing directories are reported while valid sessions restore, and partially restored Spaces stay held until a successful retry.
- Plug-in selections, grants and activity endpoints survive a reboot or logout. Each daemon keeps them in its instance directory under the data directory instead of beside its socket, whose runtime directory the system clears, and moves a journal that an earlier version kept beside the socket on first use.

### Documentation

- Documented which activity hooks and metadata fields each supported harness provides.

## [0.3.57] - 2026-11-04

This hotfix replaces 0.3.56, withdrawn because of excessive Windows pmuxd CPU
use and an Update and Restart crash.

### Fixed

- Choosing Update and Restart in the command palette keeps the app open and opens its submenu when Full Settings is enabled. Layout and Recent Messages menus also remain available.
- pmuxd no longer keeps a CPU core busy on Windows with many panes while the activity feed is on, and the activity monitor no longer times out there. On Windows, seat status takes one process snapshot for all panes instead of several per pane, and pmuxd's own session tracking takes one such snapshot per second. On every platform, activity reads within two seconds of each other share one read of the panes' processes, as long as the set of seat panes is unchanged.
- `pmux doctor` reports the running daemon version alongside the installed pmux version so an update mismatch is visible.
- Combined release checksums list only real asset names. Temporary checksum output stays outside the asset directory and the finished list replaces the old one atomically.

## [0.3.56] - 2026-11-03

Withdrawn because of excessive Windows pmuxd CPU use and an Update and Restart crash.

This release replaces 0.3.55, which was tagged but not published, and includes
its [changes](https://github.com/moonbase2090/Prismattyc/blob/v0.3.55/CHANGELOG.md#0355---2026-11-03).

### Erratum

- The published `SHA256SUMS` also names `SHA256SUMS.tmp`, which was not uploaded. Digests for the real assets are correct; skip the temporary-file entry when verifying the full list.

### Fixed

- Publication of 0.3.55 stopped because the official plug-in catalog pin was missing. Core builds now select the reviewed immutable `pmux-plugins-v1.0.0` catalog, with the same verified archive bytes across platforms.
- Release branches check for the official pin before bundle preparation. Native CI uses the same pinned bundle as public releases; unpublished guest changes use explicit candidate builds.

### Changed

- The host connects pmuxd panes and promotes them to log replicas on worker threads by default, so a slow or busy daemon no longer pauses the window while panes open. Set `PRISMATTYC_ASYNC_PANE_CONNECT=0` to keep the main-thread connect.

## [0.3.55] - 2026-11-03

### Added

- Versioned contained plug-ins for activity views and queries. Installations start disabled; enabling a plug-in selects an exact version and grants its endpoints explicitly. Settings and `pmux plugin` manage access, and workers stop when access is revoked.
- Signed asynchronous activity webhooks, time-based activity cursors, and a one-command hook installer for supported harnesses.
- The activity monitor discovers current agent sessions even when they have no recent hooks or their events have expired from history.
- Named activity seats survive daemon restarts and show whether they are working, waiting, idle, stuck, or down. Seat details show structured activity and report harness, model, and project when available.

### Changed

- The activity monitor launcher can repair its frontend on Linux and Windows without changing the six-executable core update and rollback transaction.
- Activity plug-ins must be installed and explicitly enabled after upgrading from the standalone monitor. Collection opt-in does not grant plug-in access.
- Cell glyphs and shaped emoji are rasterized once per font and reused.
- Font changes from configuration reload, zoom, and display scale load off the window event thread. Repeated changes keep only the newest pending request.
- Seat details group activity by task and pair operation starts with their results. Each action shows its outcome, available file or command, duration, and failure exit status. The details view scrolls and keeps run and operation identifiers behind the diagnostic toggle.

### Fixed

- Login restore keeps working after an agent harness updates itself by resolving a saved stable command through the restored shell's `PATH`.
- Reopening a saved Space keeps its restored panes when a snapshot refresh arrives during restore.
- Hook installation and removal reject settings files larger than 1 MiB without changing them, including when a file changes during an atomic update.
- macOS app signing signs the main executable after the helper binaries.

### Development

- Windows builds cover every workspace target with clean compiler lint checks.
- Native grid protocol types and performance baseline probes prepare for future native rendering work.
- CI checks published package links and external links, and makes pane template and daemon readiness tests more reliable.

## [0.3.54] - 2026-11-01

### Added

- An opt-in activity feed: set `experimental_activity = true` under `[mux]`, in the settings panel or the config file, and the running pmuxd starts serving seat activity metadata within a second, with no restart. Setting it back to `false` stops the feed and discards its history. It is off by default and carries no command text, paths, prompts, tool output or mail bodies.
- `pmux-activity-monitor` ships in the macOS app, the Linux tarball and the Windows ZIP and MSI, and is on PATH. It shows each seat's state and recent activity across one or more hosts, fits the pane it runs in, and explains what to do when the feed is off.
- Kitty graphics raw RGB and RGBA uploads render, as well as PNG, so pixel widgets such as the monitor's activity lanes draw as images.

### Fixed

- Windows panes again detect when their program exits, keep the last output, and clean up the session.

### Known issues

- On Linux, `pmux update` does not fetch `pmux-activity-monitor` yet. Copy it from the release tarball's `bin` directory next to `pmux`.

## [0.3.53] - 2026-10-31

### Changed

- The sidebar fold control sits beside the Spaces title, and tree rows show a right chevron when collapsed and a down chevron when expanded.
- Discrete settings cycle through their values with Enter.
- One pmuxd snapshot connection is kept on by default.

### Fixed

- The Spaces title remains visible beside the expand/collapse-all control when the sidebar is docked on either side.
- Palette panels no longer count the horizontal row inset twice when sizing to their content.
- Command+letter shortcuts no longer reach the pane as typed text.
- Focus color changes persist across restarts.
- The attached view's Space name follows a rename.
- Space panel result text stays visible after a resize.
- Spawned child processes receive only their own file descriptors (vendored portable-pty).

## [0.3.52] - 2026-10-28

### Added

- Standard macOS menus expose application actions and configured shortcuts, and a native dialog saves session configurations.

### Changed

- Session configuration restore now preserves names, working directories, and commands without replaying command text.

## [0.3.52-rc.1]

### Added

- Standard macOS menus expose application actions and configured shortcuts, and a native dialog saves session configurations.

### Changed

- Session configuration restore now preserves names, working directories, and commands without replaying command text.
- The host keeps one pmuxd snapshot connection for periodic polls by default; set `snapshot_client = false` to opt out.

## [0.3.51] - 2026-10-28

### Added

- Panes in the same tab can be reordered by dragging a pane's handle onto another pane. Dropping a pane on itself leaves the order unchanged.

## [0.3.50] - 2026-10-28

### Changed

- Complete updates now offer to save sessions, continue without saving, or cancel before anything stops. After the installed components restart, the app offers to restore the saved sessions and view, including directories, foreground commands, layout and agent bindings.
- An update restart checks the saved session configuration again right before it stops anything, stops only the saved sessions, and refuses to go ahead if the layout changed or a new session was started in the meantime, so no session is lost.
- Updates reject incomplete or mixed installations instead of reporting success.

### Fixed

- Mouse, focus and paste reporting modes are cleared when a pane's program is replaced or exits, so a restarted shell no longer receives escape-sequence junk from mouse movement.
- Pane titles skip characters the chrome font has no glyph for, so status symbols no longer draw as missing-glyph boxes.

## [0.3.49] - 2026-10-28

### Fixed

- Menus, pickers and popups now size to their longest row within the available screen width, so labels are no longer cut off with an ellipsis when there is room. When space is tight, shortened rows keep version suffixes and path basenames.

## [0.3.48] - 2026-10-28

### Added

- `pmux new` supports `--cwd DIR` to choose a session's starting directory and `--help`/`-h` to show its usage.

### Changed

- Font coverage is indexed once per font path, reducing repeated file reads during fallback lookup.

## [0.3.47] - 2026-10-28

### Added

- Clicking the Dock icon restores a minimized window, including when another window remains visible.

### Changed

- Paste handling and render-status and attach-cache writes use worker threads by default to keep the window responsive.
- Sidebar session labels stay stable when switching Spaces, and saved rows open the matching tab or pane.

## [0.3.46] - 2026-10-27

### Changed

- The host render thread bounds how much pane output it drains per pass, so a busy pane no longer holds up redraws of the rest of the window.

## [0.3.45] - 2026-10-27

### Added

- Added `pmux tab list` and `pmux tab name` commands to set, clear, and list fixed tab names.
- Mixed-case and spaced session labels can be enabled with `allow_mixed_case_session_names`; the setting defaults off.

## [0.3.44] - 2026-10-26

### Added

- An opt-in complete settings editor, enabled with `settings_panel = true`, edits every config key from the palette.

### Changed

- The transport harness reports child and reader state when a test fails.

## [0.3.43] - 2026-10-26

### Added

- The Spaces sidebar header has an expand-all and collapse-all control.

### Fixed

- Clicking a pane in an inactive Space focuses that session in that Space.
- Walkthrough "Show me" steps wait for the demonstrated action to finish before they advance.

## [0.3.42] - 2026-10-25

### Added

- pmuxd autosaves Space window layouts and preserves the saved layout when a restore is interrupted.
- `pmux space reset` resets a saved Space after confirmation and keeps a backup of the previous layout.
- A pane idle event fires once when a pane with output goes quiet, and re-arms when new output arrives.

### Fixed

- Spaces with windowless or stale saved entries now recover instead of failing to open.
- The Single, Split, and Grid arrangement controls now appear in the pane header in every window layout, including tabs, the top Space rail, and hidden tabs.

### Security

- Updated tokio and clipboard-win to clear a tokio I/O-safety advisory and a yanked clipboard-win release.

## [0.3.42-rc.1] - 2026-10-25

This build is a prerelease. A tag such as `v0.3.42-rc.1` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report the base version, `0.3.42`.

## [0.3.41] - 2026-10-22

### Changed

- Fresh installs and configs without `theme` now use Prismattyc Dark instead of following the OS appearance. `theme = "prismattyc"` still follows the OS.

### Fixed

- The app starts pmuxd before its startup flows and recreates the default Space when none exists, including when pmuxd is already running.

## [0.3.40] - 2026-10-22

### Fixed

- Create a default Space when pmuxd starts and finds none, so a new install can open a session right away. Restarts never add a second default, and a Space is recreated if all Spaces were removed.

## [0.3.39] - 2026-10-20

### Added

- Track how long a pane remains at a confirmed empty prompt, with resets for input, output, pane changes, child processes, and uncertain or busy readiness.

## [0.3.39-rc.1] - 2026-10-20

This build is a prerelease. A tag such as `v0.3.39-rc.1` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report the base version, `0.3.39`.

## [0.3.38] - 2026-10-14

### Fixed

- Installing a Windows MSI over another build of the same version now replaces the installed executables. Before, Windows Installer kept the old files because their file versions were equal.
- A quiet Windows install no longer hides a failed `pmuxd` start. The start script's warning and the `pmux up` exit code now go into the MSI log, and every install and logon start records its result in `%LOCALAPPDATA%\Prismattyc\pmuxd-start.txt`. The install still succeeds.
- On Windows, Open config with no `VISUAL` or `EDITOR` set opens `config.toml` in Notepad instead of trying to run `nano`. `VISUAL` and `EDITOR` still take precedence on every platform.

## [0.3.37] - 2026-10-14

### Fixed

- The Windows installer now adds pmux to PATH and starts the background service at install and at sign-in. Elevated installs no longer leave an unusable socket.
- A Windows MSI install prepends `%LOCALAPPDATA%\Programs\Prismattyc\cmd` to the user PATH, registers `pmuxd` to start at logon, and runs `pmux up` before the installer returns. If `pmux up` fails, the installer writes a warning and still finishes. An elevated install assigns the socket to the user instead of leaving it owned by `BUILTIN\Administrators`. A stale socket that already has that Administrators owner and the private user ACL can be replaced by a later `pmux up`. Uninstall removes that PATH entry and the logon registration. `ADDTOPATH=0` still leaves PATH unchanged and still starts `pmuxd`.

### Changed

- Walkthrough captions use sidebar row wording instead of the old rail and chip names. The tab-move step now says to move the tab down one row in the sidebar.
- Walkthrough narration is bundled. A step can set `spoken` to change what is voiced without changing its caption, so "pmux" is voiced as "P Mucks".
- Narration generation and demo recording tools are maintained outside this repository. Bundled walkthrough playback is unchanged.

## [0.3.36] - 2026-10-12

### Fixed

- The pmux-mcp tutorial uses a generic AWS profile name in its example.
- The release check scans published binaries without flagging build-runner paths, and checksums cover only regular release files.

## [0.3.35] - 2026-10-12 (not published)

### Changed

- Prismattyc is now licensed under the MIT license.

### Fixed

- `pmux doctor` prints the pane shell a new pane would start, including a configured `[mux] shell`, before it checks the daemon.
- The Windows `cmd` directory holds `prismattyc.exe`, `pmux.exe`, `pmuxd.exe`, `pmux-attach.exe`, and `pmux-mcp.exe`. Each one starts the matching program in `bin` directly. An upgrade removes the old `.cmd` shims. PATH still points at `cmd`, not `bin`. There is no host forwarder. When release signing is enabled, that forwarder is signed with the other tools and the package job verifies the five `cmd` copies.
- An unknown key in `[mux]` warns and is ignored. The known mux settings and the rest of the file stay in effect for the daemon and the host. Invalid TOML still drops the file. Unknown keys outside `[mux]`, including `[a11y]` and `[spaces]`, are still rejected.
- A pane label keeps its case, spaces, and other characters. The mailbox address is the lowercase form of that label.
- Windows line-oriented `pmux`, `prismattyc`, and `pmux-attach` output returns to column 0. Virtual-terminal processing no longer disables newline auto-return for those commands. An attached console disables auto-return only while its raw mode is open, then restores the prior mode.

### Windows

MSI PATH changes remain opt-in with `ADDTOPATH=1`. Installer verification now
checks that an opted-in upgrade leaves exactly one `cmd` entry, both commands
resolve to the `cmd` forwarders, and uninstall removes the entry.

### Windows

`[mux] shell` selects an explicit pane shell, such as `pwsh.exe` or `powershell.exe`. Leaving it unset preserves `COMSPEC` on Windows, falling back to `cmd.exe` only when `COMSPEC` is absent or blank. Unix keeps `SHELL` or `/bin/sh` when unset.

## [0.3.34] - 2026-10-10

### Fixed

- Sidebar rename of a local tab or pane opens the naming overlay. The tab strip is not painted in sidebar mode, so the old chip editor never appeared. An attached session still renames through the session overlay. IME commits type into an open tab or space name instead of the terminal. A right-click on the sidebar is not stolen by a pane cell under the pointer.
- A split pane row in the sidebar can be renamed from its menu. F2 renames the row under the pointer, or the focused space when the sidebar already has the keyboard. Space rename in the sidebar opens the naming overlay.

### Tests

- Tests open a real window and check how the space panel handles activation, typed keys, the input length cap, and clipboard paste. No user-visible change.
- Tests cover the find and theme picker overlays, including find staying closed over a fullscreen app on the alternate screen. No user-visible change.

## [0.3.33] - 2026-10-10

### Fixed

- The Windows Start menu shortcut names its icon `PrismattycIcon.exe` and takes that icon from `prismattyc-host.exe`. Windows no longer caches an extensionless icon for the shortcut.

### Tests

- A Linux test opens a real window and checks how bells, attention notices, mail counts, and window commands update it. No user-visible change.

## [0.3.32] - 2026-10-10

### Fixed

- Idle panes redraw only when something on screen changes. Rewriting the same cells, or moving the cursor back to where it started, no longer schedules another frame. A blinking caret, including a cursor-visibility sequence faster than 500ms, repaints on a 500ms timer.

### Windows

`ADDTOPATH=1` prepends the install `cmd` directory instead of `bin`.
That directory holds `prismattyc.cmd`, `pmux.cmd`, `pmuxd.cmd`,
`pmux-attach.cmd`, and `pmux-mcp.cmd`. Each shim forwards to the matching
executable in `bin`. There is no host shim. Install and uninstall remove
leftover `windows-preview-*` directories under
`%LOCALAPPDATA%\Programs\Prismattyc` when they are not reparse points,
along with their Start shortcuts and user PATH entries.

## [0.3.31] - 2026-10-09

### Windows idle CPU

An idle `prismattyc-host` on Windows no longer keeps a CPU core busy.
Resolving the mux socket applied an inheritable DACL to the runtime
directory on every call, and Windows then walked every child of that
directory on the UI thread. The directory is now secured once, and that
write does not propagate to existing children. The idle event loop waits
until the next deadline or a wake, and polls only while PTY output or a
wake is pending. macOS socket resolution does not rewrite directory
security.

The Windows foreground-process scan that identifies the agent running in
each pane now runs on a background worker instead of the UI thread.

### Appearance

The macOS DMG background uses a solid indigo drag arrow and underline.

### Testing and CI

The CRAP release gate is advisory: it reports a warning but never fails.
Legacy and Kitty disambiguate key encodings are covered by table tests.
Host transparency, side-rail clicks, and action dispatch have new tests.
The AL2023 smoke check pulls its image from the ECR public mirror. OSD
full-repaint diagnostics are clearer.

Windows packages remain unsigned while Azure Artifact Signing is pending.

## [0.3.30] - 2026-10-09

### Workspace recovery

**Start at login and restore workspace** starts pmuxd at login without a
host window and restores saved sessions, panes, tabs, and Space ownership.
Mail remains in its durable store. A supervisor restarts the daemon after
a crash, and concurrent starts cannot create duplicate sessions. The host
selects **Restore workspace** and restores after three seconds when the
setting is enabled.

New installations default to off. Existing saved workspaces default to on
at the first upgrade launch unless explicitly disabled. Restoring starts
new processes; it does not recover process memory or shell variables.
`pmux login` manages the setting, and `pmux doctor` reports its state.
Running `pmux` through a symlink now finds its companion executables.

### Rendering and responsiveness

Sustained terminal output no longer freezes painting on macOS. PTY draining
uses `pty_drain_budget_ms`, which defaults to 8 ms and can be changed live.

macOS has an optional IOSurface presentation backend. Set
`macos_present = "iosurface"` and restart to try it. The tile backend remains
the default. The new backend handles resize and scale changes and falls
back to tiles if allocation fails.

Space file reads, team descriptions, and autosave commands run on background
workers. Delayed saves stay with the Space visit that started them, and a
save reports an error if its private view cannot be written. Autosave keeps
newly moved sessions while attachment finishes.

Muse mail notifications and automatic pane submission now work when the
pane uses the Kitty keyboard protocol.

### Appearance

The Graphite sidebar uses distinct icons for Spaces, tabs, and panes in both
the expanded tree and collapsed strip. Live pane icons identify the running
agent from its foreground process. Set `sidebar_harness_icons = false` to
use generic pane icons.

Pane headers, status indicators, and animated focus rings now use shared
Graphite pane calculations. Splash lens flares paint behind the letters so
the wordmark remains legible.

### Windows

A per-user MSI installs the application and command-line tools, adds a Start
menu shortcut, and registers an Apps & features entry. Adding the tools to
PATH is optional. The installer supports upgrades and replacement builds
of the same version while leaving a running daemon alone. Detached daemon
starts no longer hold the launching command's standard handles open.

Windows builds embed the Prismattyc icon and version information in the packaged
executables, and the Start menu shortcut uses that icon. The GUI stays
`prismattyc-host.exe`. `prismattyc.exe` remains the CLI, with the file
description Prismattyc CLI, and is not a Start shortcut. Helpers the GUI starts
no longer open a console window. The login daemon starts detached, and the
installer's update-pointer script starts PowerShell hidden. The CLI tools
enable virtual-terminal processing. A pipe, `NO_COLOR`, or a console that
rejects it prints a plain page, and start and quit stay free of escape
sequences. Color follows stdout. A second bare launch of the GUI focuses the
existing window. A local daemon connect from the GUI gives up after two
seconds. The Apps & features entry points its icon at `prismattyc-host.exe`,
on the uninstall key the installer created. An elevated install writes that icon into the machine uninstall key while
it writes the registry. A non-elevated install skips that write, so a denied
machine-key update cannot roll the install back. It does not start a helper.

Windows builds in this release are not code-signed. Azure Artifact Signing
is awaiting Microsoft's organization validation, and the workflow skips
signing until it is configured. SmartScreen may warn on first run.

### Release versioning

Workspace versions now change only in release PRs. Feature and fix PRs
keep the current release version.

## [0.3.29] - 2026-10-08

### Since 0.3.3-rc.1

Graphite is now the default host chrome. The P1–P6 work moved Graphite’s
colors, token derivation, tabs, rail geometry, and sidebar layout into the
shared rendering path, pinned the graphite-core revisions used by each row,
and added the corresponding hit-testing, drag, resize, theme, transparency,
and light-cycle behavior. The sidebar supports live tabs, saved Spaces,
attention markers, collapsed mode, scrolling, arrangement controls, and
clickable chrome even when there are zero Spaces.

The dark Hive themes are lighter while preserving their contrast goals and ANSI
behavior. Theme, sidebar, and window screenshots cover the shipped palettes.

Space reopening after a pmuxd restart now reconnects existing sessions without
replaying commands into the wrong session, preserving panes, tabs, and saved
layout state. Cleanly exited attached sessions close their panes, remove stale
Space membership, and stop their daemon session; failed cleanup retries and
failed exits remain reopenable placeholders. The native regression covers the
failure-and-recovery lifecycle with a real host and daemon.

CI now installs Scorecard outside the checkout before the type and test gates,
keeps the intended diff base, and runs the release checks against the pinned
revision. The changelog’s former Unreleased sidebar-click note is included
above with the Graphite sidebar changes.

## [0.3.29-rc.2] - 2026-10-08

This build is a prerelease. A tag such as `v0.3.29-rc.2` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report the base version, `0.3.29`.

### Since rc.1

macOS Wake starvation is fixed: `Wake` is notification-only and the loop
pumps from `about_to_wait` and `RedrawRequested`, so sustained PTY output
advances the raster (0 frames before, 400 frames with 20 raster advances
after in the 20-second native harness window). Each pump drains bounded
PTY batches under `pty_drain_budget_ms` (default 8 ms, 1–1000 ms,
hot-reloads).

macOS gains an IOSurface present backend behind startup-only
`macos_present = "iosurface"`, with readback, allocation fallback, and
scale/resize reallocation. `tiles` remains the default; the backend
switches after the dogfood period. Scenario (b) present p50 falls from
21.985 ms (tiles) to 1.222 ms (IOSurface); scenario (c) shows the same
shape with the render-thread row-2 work included.

Muse mail submission works in Kitty keyboard mode: doorbells go out as
bracketed paste and `pane write --submit auto` follows the pane's live
Kitty mode instead of sending a bare CR.

Space file reads, team describe, and autosave `pmux space save` run on a
worker thread per window while the main thread queues the job and applies
the report on a later pump. A Space save now fails loudly when its
owner-only private view cannot be written, and a delayed save stays on
the visit that started it.

## [0.3.29-rc.1] - 2026-10-08

This build is a prerelease. A tag such as `v0.3.29-rc.1` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report the base version, `0.3.29`.

### Since 0.3.3-rc.1

Graphite is now the default host chrome. The P1–P6 work moved Graphite’s
colors, token derivation, tabs, rail geometry, and sidebar layout into the
shared rendering path, pinned the graphite-core revisions used by each row,
and added the corresponding hit-testing, drag, resize, theme, transparency,
and light-cycle behavior. The sidebar supports live tabs, saved Spaces,
attention markers, collapsed mode, scrolling, arrangement controls, and
clickable chrome even when there are zero Spaces.

The dark Hive themes are lighter while preserving their contrast goals and ANSI
behavior. Theme, sidebar, and window screenshots cover the shipped palettes.

Space reopening after a pmuxd restart now reconnects existing sessions without
replaying commands into the wrong session, preserving panes, tabs, and saved
layout state. Cleanly exited attached sessions close their panes, remove stale
Space membership, and stop their daemon session; failed cleanup retries and
failed exits remain reopenable placeholders. The native regression covers the
failure-and-recovery lifecycle with a real host and daemon.

CI now installs Scorecard outside the checkout before the type and test gates,
keeps the intended diff base, and runs the release checks against the pinned
revision. The changelog’s former Unreleased sidebar-click note is included
above with the Graphite sidebar changes.

## [0.3.25] - 2026-10-08

### Since 0.3.9

Attached-session cleanup remains owned by the final host window until
transient removal failures succeed, while already-destroyed sessions are
recognized as completed cleanup.

## [0.3.9] - 2026-10-07

### Since 0.3.8

When an attached session exits successfully, Prismattyc closes its pane,
removes the session from its saved Space, and tears down the live session.
Direct attachments without a saved Space are stopped by stable session ID, and
failed cleanup is retried. Failed attached clients remain available as
reopenable placeholders.

## [0.3.16] - 2026-10-07

### Since 0.3.3

The ten dark Hive themes are a little lighter. The terminal ground, chrome,
overlays, pane borders, and selection each move up 6 points of OKLCH
lightness with hue and chroma held, so the steps between surfaces stay the
same. High-Contrast Dual moves 3 points. Muted Professional moves 4.5 points,
which keeps the shared ANSI red at 4.5:1 on its ground. ANSI black follows
the ground, and bright black keeps its earlier contrast. Chrome text moves up
in the five themes where it would otherwise fall below 4.5:1. Body text,
chrome text, selection text, the cursor, and every ANSI color other than
black and bright black stay at 4.5:1 or more. High-Contrast Dual's body text
stays near 16:1. The three light Hive themes do not change.

## [0.3.3] - 2026-10-07

### Since 0.3.0

A plain click opens a link. This applies to OSC 8 links and detected
HTTP(S) URLs. The link opens on release only when the press and release stay
on the same link. Drags still select text, double- and triple-clicks still
select a word or line, and programs using mouse reporting still receive
unmodified clicks. `link_click = "modifier"` brings back Ctrl-click on Linux
and Command-click on macOS. The setting is hot-reloaded and appears in
Settings.

Right-click a space, session, or pane row in the sidebar or a vertical space
rail, or a space icon on the collapsed strip, to open a context menu. It
offers the existing open, save, rename, stop, move, and delete actions.
Destructive actions ask for confirmation. Close space… appears only for the
current space. Menu or Shift+F10 opens the menu from a focused rail, and the
unbound `space_rail_context_menu` action does the same. The sidebar and the
collapsed strip no longer show a pane row for a session with one pane. The
session row stands for that pane, and its menu adds Focus, Rename, Close
pane…, Stop…, and Move to space….

The Graphite sidebar marks the space, tab, and pane rows that need you. A
space row shows how many panes are waiting. Clicking a marker focuses the
next pane that needs attention, and opens its space first when that space is
not current. `jump_needs_you` (default `Ctrl+Shift+U`) and a command-palette
entry cycle through those panes. Attention clears when the pane prints more
output without a new attention request, so a badge no longer sticks after an
agent rings once and keeps working. Focusing the pane still clears it.

Pressing Enter on the placeholder for a saved Space session replays that
session's saved command instead of opening a bare login shell. The saved
layout and per-pane working directories come back too.
`pmux session reopen NAME --space SPACE` replays saved commands under the
same `space_open_runs_commands` policy as `pmux space open`. A seat that is
already live in an owned Space is not replayed. `--no-run` skips the
commands, and `--no-claim` reopens the session without changing the Space
file or the session's owner.

pmux mail reaches Muse panes. Doorbells used to wait because pmux did not
recognize the `muse-bin-<version>` foreground process. pmux now matches
`muse` and `muse-*` and submits with one carriage return.

Pixel alpha premultiplication is exact and faster. Each channel keeps
floor(channel × alpha / 255) for every 8-bit value, and fully opaque runs are
skipped. On a 3,528 × 1,764 frame on Apple silicon, the median opaque
conversion dropped from 2.45 ms to 0.67 ms, and alpha 128 from 2.74 ms to
2.27 ms. These times cover the conversion loop, not end-to-end presentation.

`pmux render-status --json` reports the main-thread pump phases and the count
and time of synchronous socket and subprocess waits. With `render_timer` on,
the log and the on-screen display show the pump duration and its slowest
phase, PTY parse time, and on macOS the presentation write and commit times,
tile counts, and bytes. Changed-pixel comparison follows the existing
`render_timer` modes, which default to off.

The nightly mutants in-diff job runs as 16 shards instead of one, so it
finishes inside the step timeout. Tests that could not fail now assert real
behavior. CI installs Noto CJK fonts and checks GPU adapter enumeration on
wgpu's Noop backend.

## [0.3.3-rc.1] - 2026-10-07

This build is a prerelease. A tag such as `v0.3.3-rc.1` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report the base version, `0.3.3`.

### Since 0.3.0

A plain click opens a link. This applies to OSC 8 links and detected
HTTP(S) URLs. The link opens on release only when the press and release stay
on the same link. Drags still select text, double- and triple-clicks still
select a word or line, and programs using mouse reporting still receive
unmodified clicks. `link_click = "modifier"` brings back Ctrl-click on Linux
and Command-click on macOS. The setting is hot-reloaded and appears in
Settings.

Right-click a space, session, or pane row in the sidebar or a vertical space
rail, or a space icon on the collapsed strip, to open a context menu. It
offers the existing open, save, rename, stop, move, and delete actions.
Destructive actions ask for confirmation. Close space… appears only for the
current space. Menu or Shift+F10 opens the menu from a focused rail, and the
unbound `space_rail_context_menu` action does the same. The sidebar and the
collapsed strip no longer show a pane row for a session with one pane. The
session row stands for that pane, and its menu adds Focus, Rename, Close
pane…, Stop…, and Move to space….

The Graphite sidebar marks the space, tab, and pane rows that need you. A
space row shows how many panes are waiting. Clicking a marker focuses the
next pane that needs attention, and opens its space first when that space is
not current. `jump_needs_you` (default `Ctrl+Shift+U`) and a command-palette
entry cycle through those panes. Attention clears when the pane prints more
output without a new attention request, so a badge no longer sticks after an
agent rings once and keeps working. Focusing the pane still clears it.

Pressing Enter on the placeholder for a saved Space session replays that
session's saved command instead of opening a bare login shell. The saved
layout and per-pane working directories come back too.
`pmux session reopen NAME --space SPACE` replays saved commands under the
same `space_open_runs_commands` policy as `pmux space open`. A seat that is
already live in an owned Space is not replayed. `--no-run` skips the
commands, and `--no-claim` reopens the session without changing the Space
file or the session's owner.

pmux mail reaches Muse panes. Doorbells used to wait because pmux did not
recognize the `muse-bin-<version>` foreground process. pmux now matches
`muse` and `muse-*` and submits with one carriage return.

Pixel alpha premultiplication is exact and faster. Each channel keeps
floor(channel × alpha / 255) for every 8-bit value, and fully opaque runs are
skipped. On a 3,528 × 1,764 frame on Apple silicon, the median opaque
conversion dropped from 2.45 ms to 0.67 ms, and alpha 128 from 2.74 ms to
2.27 ms. These times cover the conversion loop, not end-to-end presentation.

`pmux render-status --json` reports the main-thread pump phases and the count
and time of synchronous socket and subprocess waits. With `render_timer` on,
the log and the on-screen display show the pump duration and its slowest
phase, PTY parse time, and on macOS the presentation write and commit times,
tile counts, and bytes. Changed-pixel comparison follows the existing
`render_timer` modes, which default to off.

The nightly mutants in-diff job runs as 16 shards instead of one, so it
finishes inside the step timeout. Tests that could not fail now assert real
behavior. CI installs Noto CJK fonts and checks GPU adapter enumeration on
wgpu's Noop backend.

## [0.3.0] - 2026-10-05

Graphite is the default chrome. To keep the previous look, add this line to
`~/.config/prismattyc/config.toml`. It applies live, without a restart:

```toml
chrome_style = "classic"
```

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, `layout`, and `theme` commented out, so a later default change
reaches those installs. Classic stays selectable and tested.

This is a stable release. The tag `v0.3.0` contains no hyphen, so the publish
job does not add `--prerelease` or `--latest=false`. GitHub marks the release
Latest. `pmux update` and the app menu offer it. The binaries report `0.3.0`.

### Since 0.2.30

Unset `chrome_style` selects the Graphite tabs bar, pane title rows, spaces
bar, light-cycle focus ring, bar colors, pane-header handles, arrangement
actions, transparency dialog, and sidebar layout. The command palette, theme
picker, and other overlays use the Graphite surfaces.

Graphite chrome follows the selected theme. The tabs bar, spaces bar, side
rail, sidebar, pane title rows, chips, and overlays take their colors from
that theme. The Prismattyc themes keep their existing chrome. An explicit
`bar_color` still repaints the bars. Prismattyc Dark and Prismattyc Light
paint the chrome and the terminal from one palette. A `prismattyc` theme
follows the system appearance. `prismattyc-dark` and `prismattyc-light` pin
one side. The fresh-install template comments `theme` instead of writing
`theme = "prismattyc"`, so rolling back to 0.2.30 still loads the file.

The Space dropdown in the tabs rail opens the Space picker. Saved spaces can
be dragged into a new order on every rail, including the Graphite sidebar.
Shift+arrow moves the focused space the same way. Reordering stays off until
`space_reorder = true`. That setting defaults to false.

Saving a Space writes each tab's pane arrangement. Opening that Space puts
the splits back, including a grid, an even split, or a free-form ratio.
Spaces also autosave that arrangement when it changes. The write waits for
a short idle, then saves the live tree, including splits and ratios. Autosave
is on unless `[spaces] autosave` or the legacy `space_autosave` key turns it
off.

Graphite settings and the command palette include a Layout choice, Bars or
Sidebar. The choice applies live and is saved in the config. `layout` stays
`bars` by default.

The Graphite sidebar Arrange control shows icons for Single, Split, and Grid.
Hovering an icon shows its name. The selected icon follows the current pane
arrangement.

The Graphite transparency dialog scales its type with the window, so the
labels stay readable on a retina display. The frame stays the 760×460 design
size and scrolls inside.

Home, End, and the arrow keys follow DECCKM. With application cursor mode on,
unmodified keys use SS3. Otherwise they use CSI. On macOS, Option+Left and
Option+Right jump by word, and horizontal focus defaults to Ctrl+Option+Left
and Ctrl+Option+Right when `[keys]` does not set `focus_left` or
`focus_right`. With `macos_shortcuts = true`, Cmd+Left and Cmd+Right move to
the start and end of the line, and Cmd+Backspace sends `^U`. A config
generated by an older build that still has live `focus_left` and
`focus_right` lines keeps those chords until the lines are commented out.

The Jev shadow job labels missed mutants, scores a miss from the selected
test package, and reports a calibrated score. That score does not choose
mutants or tests. The nightly mutants in-diff job passes `--shard 0/1`,
which cargo-mutants requires whenever `--sharding` is set.

### Since 0.3.0-rc.3

The Spaces menu works with the mouse in Graphite and classic, including
while the window is translucent. Hovering a row uses the same highlight as
keyboard selection and the pointing-hand cursor. The search field uses the
text cursor. One click on a row opens that space and closes the menu. A
second click confirms a delete only on the armed row. A click outside the
panel closes the menu. The wheel and trackpad scroll the list inside the
fixed panel. A click in the search field focuses it.

With `layout = "sidebar"`, drag the grip between the sidebar and the panes
to resize it. The width is saved. It is clamped between 200 px and the room
the panes need: at least 320 px stay with the panes, and the stored width
never goes above 2000. Long names ellipsize. Double-click the grip to return
to 256 px. A chevron in the sidebar header, `sidebar_collapse`
(`Ctrl+Alt+Shift+S`), or a drag below 200 px collapses the sidebar to a
52 px icon strip. Expanding restores the last dragged width. The strip shows
an icon for each space and each session, and it scrolls inside that fixed
width. `space_rail = "right"` docks the sidebar on the right. `layout` stays
`bars` by default, so the sidebar stays off until it is selected.

Clicking a session row in the current space selects its tab and focuses that
pane. The sidebar handles the press, so the click does not type into the
pane. A session icon on the collapsed strip focuses that pane the same way.
A session row in another saved space opens that space and focuses the pane
once a live title matches. If zoom is hiding the pane, the click leaves zoom
and brings the saved arrangement back without rewriting layout ratios. A
click on the pane that is already zoomed leaves the zoom in place.

Status toasts can be turned down or off. `toasts` is `all` (the default),
`errors`, or `off`. `all` keeps the previous behavior. `errors` shows only
failures and refused requests. `off` shows no status toasts. Every status
message, shown or hidden, stays in Recent messages. Spaces settings lists
the three levels, and the unbound `recent_messages` action opens that list.
The gate runs before any painting, so Graphite and classic behave the same.

## [0.3.0-rc.3] - 2026-10-05

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, `layout`, and `theme` commented out, so a later default change
reaches those installs. Classic stays selectable and tested.

This build is a prerelease. A tag such as `v0.3.0-rc.3` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report `0.3.0-rc.3`.

### Since 0.3.0-rc.2

Graphite chrome follows the selected theme. The tabs bar, spaces bar, side
rail, sidebar, pane title rows, chips, and overlays take their colors from
that theme. The Prismattyc themes keep their existing chrome. An explicit
`bar_color` still repaints the bars.

Home, End, and the arrow keys follow DECCKM. With application cursor mode on,
unmodified keys use SS3. Otherwise they use CSI. On macOS, Option+Left and
Option+Right jump by word, and horizontal focus defaults to Ctrl+Option+Left
and Ctrl+Option+Right when `[keys]` does not set `focus_left` or
`focus_right`. With `macos_shortcuts = true`, Cmd+Left and Cmd+Right move to
the start and end of the line, and Cmd+Backspace sends `^U`. A config
generated by an older build that still has live `focus_left` and
`focus_right` lines keeps those chords until the lines are commented out.

Spaces autosave their pane arrangement when it changes. The write waits for
a short idle, then saves the live tree, including splits and ratios. Autosave
is on unless `[spaces] autosave` or the legacy `space_autosave` key turns it
off.

The Graphite sidebar Arrange control shows icons for Single, Split, and Grid.
Hovering an icon shows its name. The selected icon follows the current pane
arrangement.

The Jev shadow job labels missed mutants, scores a miss from the selected
test package, and reports a calibrated score. That score does not choose
mutants or tests.

## [0.3.0-rc.2] - 2026-10-05

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, `layout`, and `theme` commented out, so a later default change
reaches those installs. Classic stays selectable and tested.

This build is a prerelease. A tag such as `v0.3.0-rc.2` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report `0.3.0-rc.2`.

### Since 0.3.0-rc.1

The Space dropdown in the tabs rail opens the Space picker.

Saved spaces can be dragged into a new order on every rail, including the
Graphite sidebar. Shift+arrow moves the focused space the same way. The
behavior stays off until `space_reorder = true`. That setting defaults to
false.

Prismattyc Dark and Prismattyc Light paint the chrome and the terminal from
one palette. A new `prismattyc` theme follows the system appearance.
`prismattyc-dark` and `prismattyc-light` pin one side. The fresh-install
template comments `theme` instead of writing `theme = "prismattyc"`, so
rolling back to 0.2.30 still loads the file.

The Graphite transparency dialog scales its type with the window, so the
labels stay readable on a retina display. The frame stays the 760×460 design
size and scrolls inside.

Saving a Space writes each tab's pane arrangement. Opening that Space puts
the splits back, including a grid, an even split, or a free-form ratio.

Graphite settings and the command palette include a Layout choice, Bars or
Sidebar. The choice applies live and is saved in the config.

The nightly mutants in-diff job passes `--shard 0/1`, which cargo-mutants
requires whenever `--sharding` is set. The sharded jobs are unchanged.

## [0.3.0-rc.1] - 2026-10-04

Graphite is the default chrome. To keep the previous look, add this line to
`~/.config/prismattyc/config.toml`. It applies live, without a restart:

```toml
chrome_style = "classic"
```

Rolling back to 0.2.30 keeps this config working. 0.2.30 parses `chrome_style`
and the other keys added since 0.2.29. A fresh install leaves `chrome_style`,
`bar_color`, and `layout` commented out, so a later default change reaches
those installs. Classic stays selectable and tested.

This build is a prerelease. A tag such as `v0.3.0-rc.1` is published with
`--prerelease --latest=false`. `pmux update` and the app menu do not offer it.
Install the assets from the release, or opt in with `pmux update --pre`.
The binaries report `0.3.0-rc.1`.

### Graphite by default

Unset `chrome_style` now selects the Graphite tabs bar, pane title rows,
spaces bar, light-cycle focus ring, bar colors, pane-header handles,
arrangement actions, transparency dialog, and sidebar layout. The command
palette, theme picker, and other overlays use the Graphite surfaces. Screenshots
of that chrome are in `docs/design/`.

## [0.2.30] - 2026-10-04

### In-app updates

On macOS, the Prismattyc menu checks for a signed release, shows its version
and notes, and can install and relaunch. Automatic checks run on launch and
daily. Turn them off with `automatic_update_checks = false` or the menu item.
**Roll Back Last Update…** restores the previous verified app. `pmux update`
remains available on every platform.

### Graphite preview

The Graphite UI is available as an opt-in preview with
`chrome_style = "graphite"`. The default remains `classic`. A fresh install
writes `chrome_style = "classic"`. Graphite adds the redesigned tabs bar, pane
title rows, a scrolling spaces column in every rail position, the light-cycle
focus ring, Ctrl+Shift+B bar colors, pane-header drag handles, arrangement
actions, the transparency dialog, and a pointing-hand cursor on clickable
chrome. `layout = "sidebar"` is Graphite only. It replaces the bars with one
sidebar tree: space rows collapse, live rows select their tab, saved rows open
a Space, and the footer runs the same new-tab, palette, and arrange actions.
The pointer changes on those rows, and the panel scrolls.

### Command palette and dialogs

The command palette takes mouse clicks and keeps its place while the list
scrolls. The theme picker, Space pickers, session prompt, and restore prompt
use a fixed frame and scroll inside it.

### Terminal

Keyboard selection works in alternate-screen applications. A `[keys]` override
that hides a macOS Command shortcut prints a warning. Short-line scrolling,
resize reflow, and narrow-cell writes do less work.

### Jev

Jev shadow requests send AI Gateway authentication and accept a double-wrapped
gateway response (`result.result.answers`). A manual smoke workflow can make
one live call. Pull-request triage is advisory: it may add `triage:*` labels
and one comment, it stays off until `JEV_PR_TRIAGE_ENABLED` is set, and it
does not block a merge.
