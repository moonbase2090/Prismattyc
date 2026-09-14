# Recover a frozen Prismattyc mux session

A frozen pane does not prove that its session or server is stuck. The attach
client, one session, and the entire mux can present the same symptom.

Use this order. Each step increases the amount of state it destroys.

## 1. Check the server

Run these commands from another terminal:

```bash
pmux status
pmux doctor SESSION
```

`status` reports the socket, server process, and log path. `running` means the
socket accepts connections. `doctor` proves that a control request completes.

`doctor` reports each pane child, controller lease, and attach client. Attach
clients are classified as:

- `NESTED`: an attach running below a pane child.
- `VIEWER`: a host-side attach viewing the session.

Use the session name or the current ID from `pmux ls`. IDs are unique
only for this mux server's lifetime. Do not persist an ID across restarts.

## 2. Detach the viewer

For a direct terminal attach, press `C-\` and then `d`. This closes only the
client. The server, pane, and child process keep running.

In `prismattyc-host`, `Ctrl+Shift+W` closes the focused host pane. If that pane is
an attach viewer, the server-owned session still survives.

Reattach after the client exits:

```bash
pmux attach SESSION
```

If the client does not detach, continue from a different terminal.

## 3. Kick only the attach client

```bash
pmux kick SESSION
pmux doctor SESSION
```

`kick` sends `SIGTERM` to matching attach clients. It prefers nested attaches
when any exist. Otherwise, it terminates host-side viewers.

The command rechecks each process immediately before signalling it. It never
calls `DestroySession` and never signals the pane child.

Run `doctor` again after the kick. If a nested attach and a stuck viewer both
existed, a second kick may be required. Then reattach normally.

## 4. Destroy one session

Use this only when the session itself is broken and its work can be lost:

```bash
pmux ls
pmux stop SESSION
```

This sends `DestroySession`. It destroys that session's windows, pane PTYs,
and child processes. Other sessions and the mux server remain running.

A numeric-only name can collide with another session's ID. `stop` fails closed
when that happens. Do not guess or hand-signal a pane. Prefer unique,
non-numeric names when creating sessions.

## 5. Stop the entire mux

Use a server stop only when `ls` and `doctor` cannot complete a control
request:

```bash
pmux stop
```

A bare `stop` ends every session. It is not detach, and Prismattyc does not restore
those sessions after restart.

Do not hand-signal the server first. `stop` already uses this escalation:

1. Send `ShutdownServer` over the control socket.
2. Wait two seconds for the process and socket to disappear.
3. Send `SIGTERM`, then wait another two seconds.
4. Recheck the exact process arguments before sending `SIGKILL`.

The argument check protects against signalling a recycled process ID. A
server started through an unrecognized wrapper may not match. In that case,
`stop` refuses to guess and asks for manual intervention.

## Recover after a signal kill

Graceful shutdown removes the socket. `SIGKILL` cannot run that cleanup, so a
stale socket may remain:

```bash
pmux status
pmux up
```

`status` identifies a stale socket. `up` replaces only a socket classified as
stale. Prismattyc refuses foreign or undiagnosable sockets.

## Why the pane looked frozen

Two attach failures produced this symptom on 2026-08-16:

1. A user ran `pmux attach` inside a mux pane. The nested attach captured
   that pane's PTY. The outer viewer consumed the detach chord.
2. A host-side viewer stopped accepting detach input. The session and its
   server-owned child remained healthy behind that viewer.

Avoid starting an attach inside a mux-owned pane. Use another terminal or
`pmux attach --all` for a host-side view.

## Safety traps

- A PID shown for a pane is the pane child, often a shell. It is not
  necessarily the agent process.
- `DestroySession` requires no pane lease. Any same-user client that can open
  the private control socket can destroy any session.
- Killing the mux server kills and reaps its pane children.
- Server termination is not detach. There is no cold session resurrection.
- Mux IDs are server-scoped counters, not UUIDs or durable identities.

## Incident example

The `cursor-la` session froze on 2026-08-16. The operator destroyed only that
session and created a replacement. `cursor-qa` and the mux server survived.

At the time, Prismattyc had no `doctor` or `kick`. With the current CLI, inspect and
kick the attach layer before using the destructive session stop.

See [mux-cli.md](mux-cli.md) for command details and
[ADR-0011](adr/0011-long-lived-mux-server.md) for detach lifetime guarantees.
