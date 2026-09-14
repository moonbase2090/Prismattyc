# SSH attach from macOS

**Status:** First slice landed (2026-08-22). `pmux attach` is the SSH TTY
path. `pmux space attach` / `pmux space open --tty` restore a space in
that TTY (PT-99). `pmux attach --all` requires a Linux display and does
not run over SSH. This is not a remote-host support claim.

Operator command reference: [mux-cli.md — SSH from macOS](mux-cli.md#ssh-from-macos).

**Depends on:** [ADR-0011](adr/0011-long-lived-mux-server.md) and
[mux-cli.md](mux-cli.md). Remote transport remains Later under PRD §2.8.1.

## Use the TTY attach client

Use `pmux attach` from the SSH terminal. It connects to the same-user Unix
socket. It does not need a local compositor.

```bash
ssh linux-rig
pmux status
pmux ls
pmux attach SESSION
pmux space attach NAME
```

Detach with `C-\ d`. The pane child continues to run in `pmuxd`.

Do not use `pmux attach --all` over SSH. That command starts
`prismattyc-host`, which needs `WAYLAND_DISPLAY` or `DISPLAY` on Linux.
Do not forward Wayland or X11 to run the Linux host on the Mac.

## Use the correct socket

The default socket is:

```text
$XDG_RUNTIME_DIR/prismattyc/pmux.sock
```

When `XDG_RUNTIME_DIR` is unavailable, pmux uses:

```text
/tmp/prismattyc-<uid>/pmux.sock
```

A graphical login commonly uses `/run/user/<uid>`, while an SSH login may
use a different runtime directory or no directory. If `pmux status` reports
no server while the desktop mux is running, set the correct socket explicitly:

```bash
export PMUX_SOCKET=/run/user/$(id -u)/prismattyc/pmux.sock
pmux attach SESSION
```

The CLI reports live `pmux*.sock` sockets under `/run/user/<uid>` and its
`prismattyc/` subdirectory when it detects this runtime-directory mismatch.
It never selects another socket automatically.

## Later work

A later windowed remote client can run `prismattyc-host` on macOS and connect
to a forwarded Unix socket. Linux keeps `pmuxd` and the pane processes. The
Mac owns the GPU, window, clipboard, and input.

Do not start this work until the SSH TTY path is proven on the target Mac.

## Acceptance

- `pmux attach SESSION` paints and accepts keys over SSH.
- Detach does not stop the pane child.
- `pmux attach --all` over SSH does not start `prismattyc-host`.
- A wrong runtime directory produces a diagnosed miss.
- No new control-plane verbs are required.

## Non-goals

- Remote multi-machine support as a Phase 2 product claim.
- Changes to same-user `SO_PEERCRED` or 0600 socket authentication.
- Windows OpenSSH support.
- A default macOS host that dials a Linux mux.
