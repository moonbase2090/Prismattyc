# Connect to a session over SSH

Use SSH to attach to a session running on another computer. This displays
the remote session inside your current terminal.

1. Install the `pmux` tools on the remote computer.
2. Make sure `pmux` is on the remote user's `PATH`.
3. Start a session on that computer with `pmux up` and `pmux new work --no-attach`.
4. Connect from your local terminal:

   ```bash
   ssh -t user@host 'pmux attach work'
   ```

Replace `user@host` with the remote login and `work` with the session name.

To detach, press **Ctrl+\\**, release the keys, then press **d**.
The remote session keeps running while its `pmuxd` process is running.

This connection uses SSH's terminal transport. It does not forward a native
Prismattyc desktop window or provide a direct network listener for `pmuxd`.
See the [pmux reference](mux-cli.md) for attach options and
[troubleshooting](hung-session-recovery.md) for an unresponsive session.

## Terminal database on the remote

SSH sends the terminal name when it allocates a PTY, but does not copy the local
terminal database. A server without the Prismattyc entries can report
“unknown terminal type” when running `clear`, `less`, or `vim`.

Explicitly install the portable entries once for your remote user (requires
`tic`, usually provided by the server's ncurses package):

```bash
# From a source checkout:
./scripts/install-prismattyc-terminfo.sh --ssh user@host
# From the macOS app, adjust the installation location if necessary:
/Applications/Prismattyc.app/Contents/MacOS/install-prismattyc-terminfo.sh --ssh user@host
ssh user@host
```

The helper uses your normal SSH authentication, host-key verification and
`~/.ssh/config`. For a custom port, jump host or other options, create an SSH
host alias and pass that alias. It compiles only the shipped portable source
into the remote user's `~/.terminfo`; it needs no sudo and makes no system-wide
changes. It does not install automatically on every connection. Repeat after a
terminal capability update. Invoking the helper without arguments installs into
the local user's `~/.terminfo` instead.

If you cannot install remote entries, use a standard terminal identity for that
connection:

```bash
TERM=xterm-256color ssh user@host
```

This uses the remote system's standard entry and does not advertise Prismattyc
extensions. To verify an installation, run `infocmp "$TERM"`, `tput colors`,
and `clear` inside the remote session, then open `less` and `vim` normally.

### Isolated regression

From the repository root, with the shared heavy-work slot available:

```bash
docker build -f tests/terminfo/Dockerfile -t prismattyc-terminfo-test .
docker run --rm --network none prismattyc-terminfo-test
```

The disposable container checks bundle lookup with an empty home, both compiled
formats, actual `tic` installation, and SSH to a fresh localhost account followed
by `clear`, `less`, and `vim`. It uses container-only keys and no mounted host
credentials. Run `python3 scripts/terminfo_test.py` for local packaging/compiler
checks alone; that command explicitly skips the SSH fixture. Native Mac checks
are documented in [macOS setup](macos.md#child-terminfo).
