# Update and restart Prismattyc

Open the command palette. Select **update_restart**. You can check for an
update, install a release, roll back, inspect versions, or restart components.
The same controls are available through `pmux`.

## Check and install a release

```bash
pmux update --check
pmux update
pmux versions
```

The release channel starts at **0.2.0** in **Moonbase2090/Prismattyc**.
That repository will start with fresh git history. Update uses release
versions and assets. It does not need a checkout or shared commit history.
Before the first release exists, the check reports that no usable release
is available. It leaves your installation unchanged.

Update accepts stable, immutable releases from the configured repository.
It downloads the six binaries for your platform. It verifies asset names,
origins, sizes, SHA-256 digests, and reported versions. All checks must pass
before activation. One atomic link change activates the complete set.
The update lock prevents concurrent installations and rollbacks.

GitHub supplies the metadata over HTTPS. This verifies integrity against
that metadata; it is not an independent signature verification or a full
TUF implementation. Release maintainers must protect the publishing account
and enable immutable releases.

The release installer manages a complete existing binary installation.
It keeps version directories under `$XDG_DATA_HOME/prismattyc/updates`
(default: `~/.local/share/prismattyc/updates`). It preserves the original
binaries for rollback. A failed or interrupted download does not activate
partial binaries. Retry `pmux update` after correcting the reported problem.
Use `--bin-dir /absolute/path` to select the installation when running from
a build tree. An incomplete existing installation must be repaired first.

Linux release assets use `x86_64-unknown-linux-gnu` or
`aarch64-unknown-linux-gnu`. macOS command-line installations use the
corresponding Apple target. Updating these binaries does not replace a
separately installed macOS application bundle. Distribute a signed and
notarized bundle through the macOS release process.

To restore the previous complete installation:

```bash
pmux update --rollback
```

Update and rollback change installed binaries. Running processes retain
their current version until they restart. `pmux versions` reports both.
MCP reports the adapter version separately from its long-lived supervisor.
Restart updated hosts before upgrading a daemon from a pre-reflow version.
New hosts replay resize events from older daemons with their original grid
semantics. Reflow starts when the daemon also runs the new build.

## Restart components

```bash
pmux restart --plan
pmux restart
pmux restart --host
pmux restart --mcp
pmux restart --daemon
```

| Component | Restart behavior |
| --- | --- |
| Host | Save window views, replace the host process, and reconnect to live mux sessions. Defer if a window owns blank terminals, has an unfinished Space operation, or displays an exact secondary pane that the session-based cache cannot restore. |
| MCP | Restart registered supervised adapters. Keep the client connection. Report interrupted requests; never replay tool calls. |
| Daemon | Restart only when there are no sessions. Otherwise report a deferral. |

The default restarts every safe component. A deferred component remains
running. Close blank terminals before restarting their host. Older hosts
and unsupervised MCP adapters must be restarted through their launcher.
The daemon checks that it is empty atomically and refuses later requests
once shutdown starts. An older daemon cannot perform this safe idle restart.
Use its existing stop procedure or explicitly select `--stop-sessions`.
A timeout means that the result is unknown. Inspect the reported receipt
and running versions before attempting another restart.

To deliberately stop every daemon session and restart it:

```bash
pmux restart --daemon --stop-sessions
```

This command terminates running programs in every session. It starts a
detached helper so the restart can finish even when the calling pane closes.
The command reports the helper PID and log path. The UI does not select
this destructive option automatically.

## Build from development source

```bash
pmux update --source --all
pmux update --source --host
pmux update --source --mux
```

`--source` explicitly selects the developer workflow. It pulls and builds
source. Ordinary Update uses release artifacts.


## Uninstall

`pmux uninstall` (equivalently `prismattyc uninstall`) is one shared command,
like `update`. It removes everything Prismattyc installed or wrote on this
machine, on macOS, Linux and Windows.

```bash
pmux uninstall --dry-run     # list what would be removed; remove nothing
pmux uninstall               # remove everything, after a prompt
pmux uninstall --yes         # remove everything, no prompt
pmux uninstall --keep-data   # remove everything except Spaces and config
```

Behavior:

- The daemon and sessions are stopped first, after a printed warning. A
  runtime directory that still hosts a live daemon is **refused** (never
  deleted) with an error telling you to stop it first — uninstall can never
  pull a live socket out from under a running daemon.
- The full list of paths is printed, grouped by category, before anything is
  removed.
- Without `--yes`, it asks for confirmation. `--yes` (or `-y`) skips the
  prompt. In a non-interactive context, confirmation is required, so pass
  `--yes` in scripts.
- `--dry-run` (or `-n`) prints the plan and exits without removing anything.
- The default removes everything, **including** Spaces and config data.
  `--keep-data` keeps Spaces, layouts, the mailbox, session state and
  `config.toml`.
- Anything that cannot be removed is reported with **what** could not be
  removed, **why**, and the **fix**; every other item is still removed.
- The exit status is non-zero if anything is left behind. The command ends by
  confirming Prismattyc is fully gone, or by naming the leftovers.

Path safety: only paths in the computed inventory are removed. There is no
globbing outside Prismattyc's own directories, and symlinks are removed as
links — the target a symlink points at is never followed or deleted. Host-global
paths (`/Applications`, the shared `/tmp/prismattyc-<uid>` runtime dir) are
included only when your user directories are at their real defaults; an
invocation with a redirected (sandboxed) `HOME`/XDG never targets the real
system app or the shared runtime dir. Empty `prismattyc/` parent directories
inside a shared config or data home may remain; they hold nothing and are safe
to leave or remove by hand.

### What is removed (inventory)

The inventory is computed from the same base directories the installer and
self-updater use (`$HOME`, `$XDG_CONFIG_HOME`/`%APPDATA%`,
`$XDG_DATA_HOME`/`%LOCALAPPDATA%`, `$XDG_RUNTIME_DIR`, `$CARGO_HOME`), so it
follows your environment overrides.

| Category | Paths |
| --- | --- |
| Binaries / launchers | `pmux`, `pmuxd`, `pmux-attach`, `pmux-mcp`, `prismattyc`, `prismattyc-host` in `~/.local/bin` and `$CARGO_HOME/bin` (`~/.cargo/bin`) |
| App bundle (macOS) | `~/Applications/Prismattyc.app`, `/Applications/Prismattyc.app` |
| Install tree (Windows) | `%LOCALAPPDATA%\Programs\Prismattyc`, `%LOCALAPPDATA%\Prismattyc\run` |
| Self-update store | `$XDG_DATA_HOME/prismattyc/updates` (versions, `current`/`previous`/`legacy` links, `installation.json`, `update.lock`) |
| Runtime state | `$XDG_RUNTIME_DIR/prismattyc/` and `/tmp/prismattyc-<uid>/` (sockets, `*.pid`, `*.log`, `*.host.pid`, `*.host.ack`) |
| Data (`--keep-data` keeps) | `$XDG_DATA_HOME/prismattyc/{mail.db,session-agents.json,walkthrough.json,spaces/,layouts/}` |
| Config (`--keep-data` keeps) | `$XDG_CONFIG_HOME/prismattyc/config.toml` |
| OS integration | Linux `.desktop` entry and hicolor icons under `$XDG_DATA_HOME`; `~/.config/systemd/user/pmuxd.service`; man pages under `$XDG_DATA_HOME/man/man1`; `~/.terminfo/p/prismattyc*` |

### Manual uninstall

If the command is unavailable (for example the binaries are already gone), the
same inventory can be removed by hand. Adjust the base directories for your
environment overrides.

macOS / Linux:

```bash
# Stop the daemon first.
pmux stop 2>/dev/null || true

# Binaries and launchers.
rm -f ~/.local/bin/{pmux,pmuxd,pmux-attach,pmux-mcp,prismattyc,prismattyc-host}
rm -f ~/.cargo/bin/{pmux,pmuxd,pmux-attach,pmux-mcp,prismattyc,prismattyc-host}

# macOS app bundle.
rm -rf ~/Applications/Prismattyc.app /Applications/Prismattyc.app

# Self-update store, data and config.
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
rm -rf "$DATA/prismattyc/updates"
rm -rf "$DATA/prismattyc/spaces" "$DATA/prismattyc/layouts"
rm -f  "$DATA/prismattyc/mail.db" "$DATA/prismattyc/session-agents.json" "$DATA/prismattyc/walkthrough.json"
rm -f  "$CONFIG/prismattyc/config.toml"

# Runtime state.
rm -rf "${XDG_RUNTIME_DIR:-/tmp}/prismattyc" "/tmp/prismattyc-$(id -u)"

# OS integration (Linux desktop entry, icons, systemd unit, man pages, terminfo).
rm -f "$DATA/applications/prismattyc-host.desktop"
rm -f "$DATA"/icons/hicolor/*/apps/prismattyc.png "$DATA"/icons/hicolor/scalable/apps/prismattyc.svg
rm -f "$CONFIG/systemd/user/pmuxd.service"
rm -f "$DATA"/man/man1/{prismattyc,prismattyc-host,pmux,pmuxd,pmux-attach,pmux-mcp,pmux-pane-write}.1
rm -f ~/.terminfo/p/prismattyc ~/.terminfo/p/prismattyc-host
```

Windows (PowerShell):

```powershell
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\Prismattyc"
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Prismattyc\run"
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\prismattyc\updates"
Remove-Item -Force "$env:APPDATA\prismattyc\config.toml"
# Remove the Start Menu shortcut and the user PATH entry that the preview installer added.
```
