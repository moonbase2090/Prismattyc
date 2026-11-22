## [0.3.73] - 2026-11-21

### Added

- Experimental native terminal grids and uploaded images for direct local PTY applications on supported Linux and Apple Silicon macOS hosts, enabled with `--experimental-rich`. Native content supplies rendering and text readers; release, resize or lease expiry restores the retained ANSI terminal. The feature remains off by default.

### Changed

- `pmux update` (and `prismattyc update`) is now the one update command: it installs the newest release, includes its official monitor launcher on Linux and Windows, restarts what can restart without stopping a session, and prints one receipt that names anything left waiting and what to do next. `--no-restart` installs without restarting. `pmux update --json` keeps its install-only report; add `--restart` or `--no-restart` for the complete update with a JSON receipt. On Linux and Windows, clicking the update notice opens the **Restart to update** review, and **Update and restart** in the app also includes the monitor launcher.
- Every dialog, picker and modal menu (the command palette, settings, the theme picker, Transparency, Update and restart, confirmations and the rest) now dims the app behind it and blurs it slightly, so the dialog stands out. The backdrop fades in; with `reduced_motion = true`, or the system's Reduce motion (macOS), Animation effects off (Windows) or GNOME animations off, it appears at once, and with the system's Reduce transparency (macOS), Transparency effects off (Windows) or GNOME's high-contrast theme it dims without blurring. On a very large window, or where the blur would slow the frame, it only dims. Clicking outside a dialog works as before.
- **Update and restart** shows each step of the restart (saving or stopping the sessions, stopping the old daemon, starting the new one, checking the restored sessions, reconciling the official plug-ins, the MCP adapter and the relaunch), and the relaunched app continues the same progress: it opens the windows, then shows **Update finished** with each step's outcome until you close it. A failed update shows the same list with the step that failed. A session started while the dialog is open now fails on **Checking the sessions** by name. `pmux restart` prints the same steps on stderr, and `pmux restart --progress` reports them as progress lines; `pmux update` also prints its verify and install steps.

### Fixed

- `pmux update` no longer says Updated and exits 0 when official plug-in reconciliation failed. The receipt names each failure and the command exits non-zero.
- An open app window with nothing to draw stopped renewing its registration, so `pmux restart` and **Update and restart all components** skipped it as if no window were open. Windows now renew it while idle, and `pmux restart` reports a window from an older release that stopped answering as `idle` instead of leaving it out.
- **Restart components** no longer fails with "expected" the newest release's version when the installed Prismattyc is newer than that release; it restarts on the installed version.
- On Linux and Windows, a key held while a window gains focus is no longer applied twice. One Escape pressed as the app reopened after an update closed both the update dialog and the result notice under it.
- **Update and restart all components** now restarts every open Prismattyc app process, not only the one you used, and the result lists any window that is still on the previous version with the exact next action. `pmux restart --host` accepts `--except-pid` to leave one app process for last.

