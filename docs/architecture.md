# Architecture

Prismattyc separates the running terminal sessions from the windows that
display them. The background server, `pmuxd`, owns managed sessions and
their child processes. Desktop windows and terminal attach clients connect
to that server.

## Data flow

```text
Shell or program
       ↕ PTY input and output
pmuxd: terminal parser, screen state, sessions, and pane layouts
       ↕ local control socket
prismattyc-host or pmux-attach
       ↕ keyboard, mouse, and display
User
```

A managed pane has its own pseudo-terminal (PTY). The child program writes
terminal escape sequences to the PTY. The emulator parses those bytes and
updates the screen model. A client displays the resulting state and sends
input back through the server.

## Session ownership

The server owns managed session topology: sessions contain windows, and
windows contain panes. Moving a managed pane changes its membership without
restarting its child process.

Detaching a client leaves the server and its sessions running. Stopping the
server ends those sessions. Saved Space layouts describe how to restore a
workspace; they are not process snapshots.

The desktop application also supports local blank terminals. Their restore
option saves layout and directory information and starts fresh shells.
See [Sessions and Spaces](spaces.md).

## Screen model

`prismattyc-core` stores terminal cells, styles, cursor state, scrollback,
and changed regions. `prismattyc-emulator` applies terminal input to that
model. The cell grid maps logical rows to physical storage so scrolling can
move row indices instead of copying the whole screen.

Scrollback has row and byte limits. Once history is full, outgoing lines
can reuse the oldest stored row's allocation. Resizing reflows text while
preserving the logical row order.

## Presentation

`prismattyc-host` draws the terminal and its controls into a CPU framebuffer.
It selects a presentation backend for the platform. The nested
`prismattyc` executable renders into an existing terminal instead.

See [Rendering](rendering.md) for backend and transparency behavior.

## Control and application protocols

The local control plane uses versioned JSON messages over a same-user Unix
socket. Clients receive snapshots and events. Controller leases prevent
multiple clients from writing conflicting input to a pane.

Applications can negotiate optional rich-content capabilities. Programs
that do not negotiate those capabilities continue to use ordinary terminal
input and output. The protocol types live in `prismattyc-protocol`; the
application-side library is `prismattyc-rich-client`.

See the [pmux reference](mux-cli.md), [capability protocol](capability-protocol.md),
and [rich-client guide](rich-client.md) for those interfaces.
