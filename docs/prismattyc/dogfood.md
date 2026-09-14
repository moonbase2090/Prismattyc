# Dogfooding the fold

How the agents run their mail loop against `pmuxd` only, with the legacy
`switchboardd` off.

## Daemons

One `pmuxd` is the end state. Start it on the default socket:

```bash
pmux up
```

`pmux up` binds `$XDG_RUNTIME_DIR/prismattyc/pmux.sock`. Use `--socket`
or `PMUX_SOCKET` only when you intentionally run a non-default instance.

For boot durability use `contrib/pmuxd.service` (systemd user unit).

## Agent mailboxes

Agents that drive mail from the CLI or MCP get headless sessions — a
mailbox with no pane:

```bash
pmux new --headless operator-id   # mailbox address defaults to the name
pmux new --headless operator-id
```

Mail to a headless agent queues; no doorbell injects (there is no pane).
`pmux mail who` lists the bound agents.

## The watcher recipe

```bash
export PMUX_SOCKET=/run/user/1000/prismattyc/pmux-mail.sock  # if not the default socket
while pmux mail --as operator-id watch; do
  pmux mail --as operator-id claim --json
done
```

`watch` blocks server-side on the mailbox condvar and exits 0 when mail
arrives, 1 on timeout. No external doorbell script, no polling loop.

## MCP hosts

`pmux-mcp` serves the nine `pmux_*` tools over the Mail* protocol:

```json
{
  "mcpServers": {
    "pmux-mcp": {
      "command": "/home/brandan/.local/bin/pmux-mcp",
      "args": ["--as", "operator-id"],
      "env": { "PMUX_SOCKET": "/run/user/1000/prismattyc/pmux-mail.sock" }
    }
  }
}
```

Identity order is `--as`, then `$PMUX_AGENT`.
Socket order is `$PMUX_SOCKET`, then `$XDG_RUNTIME_DIR/prismattyc/pmux.sock`.

## Cutover checklist

Completed on the Prismattyc host. Recorded for other hosts:

1. `cargo install --path crates/prismattyc-mux` and
   `cargo install --path crates/pmux-mcp`
   (pmux, pmuxd, pmux-attach, pmux-mcp).
2. Start `pmuxd`; create headless sessions for each agent.
3. Every agent loop sets `PMUX_SOCKET` if the mail instance is not the
   default socket.
4. Watchers use `pmux mail watch` (the `switchboard` shim is gone as of
).
5. Both agents confirm send/claim/commit through `pmux mail`.
6. `systemctl --user disable --now switchboardd` — the legacy daemon is
   no longer started (Phase 2 exit).
7. Remove the legacy unit and binary:
   `rm ~/.config/systemd/user/switchboardd.service ~/.local/bin/switchboardd`,
   then `systemctl --user daemon-reload`.
8. Archive `~/.local/share/switchboard/mail.db` if you need its historical
   data. `pmuxd` does not import the legacy database.
