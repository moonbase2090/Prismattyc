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
`aarch64-unknown-linux-gnu`. Update downloads those six binaries.
It does not download or replace `Prismattyc.app`.

The Apple silicon app is a separate release asset,
`Prismattyc-vX.Y.Z-macos-arm64.zip`. Open that zip and move
`Prismattyc.app` to the Applications folder. A later `pmux update` does
not replace that bundle. Intel and universal Mac archives are not published.

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
