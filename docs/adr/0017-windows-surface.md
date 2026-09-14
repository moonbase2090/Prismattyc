# ADR-0017 — Windows surface (control transport and cfg gates)

- **Status:** Proposed (PT-180; design only)
- **Date:** 2026-09-02
- **Depends on:** ADR-0008, ADR-0011, [agents.md](../agents.md) Windows CI guard
- **Amends:** none. Do not change the Unix socket contract on Unix.

## Context

PT-101 added Wine jobs for the portable crates. These four crates stay
out of that job until they rustc for `x86_64-pc-windows-gnu`:

| Crate | Why it fails today |
| --- | --- |
| `prismattyc-mux` | `pmux` / `pmuxd` / `pmux-attach` import Unix sockets and PTY types |
| `prismattyc-host` | `attach_log` uses `UnixStream`; `main.rs` names unix-only mux symbols |
| `prismattyc` | Classic `expand_empty_paste` is a unix-only mux export |
| `pmux-mcp` | Mail* `UnixStream` |

The JSON control protocol (ADR-0008) can stay. The Unix socket and the
same-uid `SO_PEERCRED` check cannot. Pane I/O is a second problem:
Unix uses a PTY; Windows uses ConPTY. Do not mix those two problems.

Do not stub a fake mux. Implement a real Windows transport, or keep
the crate excluded.

## Decision (proposed)

### D1 — Split control I/O from pane I/O

| Plane | Unix today | Windows first cut |
| --- | --- | --- |
| Control (`ControlServer`, Mail*, attach-log) | `UnixStream` at `default_socket_path` | Named pipe |
| Pane child | PTY | ConPTY through existing `portable-pty` |

Keep newline-delimited JSON, `version`, `request_id`, leases, and
resnapshot rules. The byte pipe, peer-identity check, and
`probe_socket_liveness` mapping change. The four ADR-0008 states
(Missing / Live / Stale / Foreign) stay.

### D2 — Named pipe is the Windows control transport

Compare:

| Option | Keep | Drop as default |
| --- | --- | --- |
| Named pipe | Local, no TCP port | — |
| TCP loopback | Easy `TcpStream` | No peer uid; port clash; firewall prompt |
| AF_UNIX on Windows 10+ | Same path shape | Wine and mingw support is uneven |

Use a small `ControlIo` trait in `prismattyc-mux` (`connect`,
`listen`, `accept`, `pair`, `peer_is_same_user`, `probe`). Unix
keeps `UnixStream`. Windows uses a named pipe. Do not add TCP as
the default.

**Names.** `validate_socket_instance` stays (1..=48 ASCII
alphanumeric, `-`, `_`). Default instance is `\\.\pipe\pmux`.
A named instance is `\\.\pipe\pmux-<instance>`.
`default_socket_path` keeps that function name and returns the
pipe name on Windows.

**Lifecycle.** A named pipe has no leftover file. The name goes
away when the last server handle closes. Do not unlink. Bind only
when the probe is Missing. Use `FILE_FLAG_FIRST_PIPE_INSTANCE` so
a squatter cannot pre-create the name. A connect that opens must
still pass the versioned handshake before the probe returns Live
(same rule as Unix / macOS backlog).

| ADR-0008 state | Windows detection | Cleanup / replace |
| --- | --- | --- |
| Missing | `CreateFile` → `ERROR_FILE_NOT_FOUND` | Bind with `CreateNamedPipe`. No unlink. |
| Live | `CreateFile` opens, or `ERROR_PIPE_BUSY` (then `WaitNamedPipe` and retry), and handshake answers | Do not replace. |
| Stale | `CreateFile` opens but handshake times out or EOFs | Close the client handle. Do not steal the name. Next bind uses `FILE_FLAG_FIRST_PIPE_INSTANCE`. |
| Foreign | `CreateFile` → `ERROR_ACCESS_DENIED`, or `CreateNamedPipe` + `FIRST_PIPE_INSTANCE` fails because another process holds the name | Do not replace. |

**ACL at create, not only after connect.** The server must set
`SECURITY_ATTRIBUTES` whose DACL grants the current user SID only,
plus `PIPE_REJECT_REMOTE_CLIENTS` and
`FILE_FLAG_FIRST_PIPE_INSTANCE`. A post-connect SID check is
defence in depth. It is not the Unix `0600` equivalent. Without
the DACL, a foreign client can connect and consume an instance
before the server rejects it.

**Sibling files.** `{stem}.host.pid`, `{stem}.host.ack`, and
`{stem}.attach-tabs.json` stay plain files. They cannot live
beside `\\.\pipe\…`. Write them under
`%LOCALAPPDATA%\prismattyc\` as `pmux.host.pid` /
`pmux-<instance>.host.pid` (and the same stem for `.ack` and
`.attach-tabs.json`).

### D3 — cfg-gate by crate, not one giant `cfg(unix)` crate

| Symbol | Unix | Windows |
| --- | --- | --- |
| `ControlServer` / `ControlRequest` | Keep | Keep; I/O through `ControlIo` |
| `UnixStream` at call sites | Keep | Remove. Use `ControlIo`. |
| `default_socket_path` | `$XDG_RUNTIME_DIR` / `/tmp` | `\\.\pipe\pmux` or `\\.\pipe\pmux-<instance>` |
| `probe_socket_liveness` | Socket file + connect + handshake | Table in D2 |
| Same-user check | `SO_PEERCRED` after `0600` | DACL at `CreateNamedPipe`; SID check after accept |
| `expand_empty_paste` / `image_paste` | Keep | Leave unix-only until a clipboard ticket |
| `host_register`, attach-tabs | Beside the socket | `%LOCALAPPDATA%\prismattyc\` plain files |
| Wayland, rustix termios, `procinfo` | Keep | Stay unix-only |

Bins (`pmux`, `pmuxd`, `pmux-attach`) and `pmux-mcp` `mail.rs` must
not import `std::os::unix`. `attach_log` uses `ControlIo` the same
way. Classic may compile without `expand_empty_paste` on Windows
(plain paste only).

### D4 — Lift PT-101 exclusions in this order

1. `prismattyc-mux` lib: `ControlIo` + `ControlServer` +
   `default_socket_path`. Then the three bins. Lift mux from CI.
2. `pmux-mcp` (`mail.rs`). Lift mcp from CI.
3. `prismattyc-host` (`attach_log` + unix-only mux symbols). Lift host.
4. `prismattyc` classic last, or keep it excluded if the owner leaves
   `expand_empty_paste` unix-only.

One crate per follow-up ticket. Each lift must rustc and run the
Wine test `.exe` set for that crate. Fail if MinGW or Wine is
missing. Do not allow-fail PTY tests; keep those `cfg(unix)` until
ConPTY tests exist.

### D5 — What stays Unix-only in the first Windows cut

- `SO_PEERCRED` / `getpeereid`
- Wayland / X11 host present, rustix termios, systemd runtime dir
- `procinfo` process scan
- Kitty `t=f` owner-uid file transport (already unix)
- Image paste via `image_paste` (X11/Wayland clipboard)

`pmuxd` on Windows may own ConPTY panes. That is in scope after
step 1, not before the control pipe exists.

## Open questions for the owner

1. Confirm named pipes as the Windows control transport (not TCP, not
   AF_UNIX-first).
2. Is the create-time DACL plus post-connect SID check enough, or do
   you also want a handshake token?
3. Must the first Windows `pmuxd` host ConPTY panes, or is compile +
   control-only enough for the first lift?
4. Keep classic `expand_empty_paste` unix-only, or schedule a Win32
   clipboard follow-up?
5. After each lift, is Wine still the merge gate, or do we wait for
   hosted `windows-latest` MSVC? Wine named-pipe ACL fidelity is
   unknown. A native Windows ACL check is required before the mux
   lift merges.

## Consequences

Unix behaviour does not change. PT-101 jobs stay as they are until
a follow-up lifts one excluded crate. Implementation tickets must
cite this ADR and the owner's answers. Do not treat a Wine-only
ACL pass as enough for the mux lift.
