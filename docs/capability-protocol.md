# Capability discovery protocol

How an application asks Prismattyc which optional features a pane supports
without changing the classic terminal path.

The only feature family a host answers today is native sessions and images
(see [native sessions and images](native-lunatui-backend.md)). Rich mode, its
overlays, docked workspace, status layer, collections and semantic copy were
removed; their messages are no longer decoded and hosts ignore them.

Related: [architecture.md](architecture.md) and `prismattyc-protocol` in
[workspace.md](workspace.md).

## Invariants

1. Classic VT/xterm behavior is always available and never gated on Prismattyc.
2. An application must receive an affirmative capability reply before it uses
   a Prismattyc-specific feature.
3. No reply, a malformed reply, or a timeout means classic-only for that
   session.
4. Capability parsing is bounded and must never stall PTY processing.
5. A failed negotiation cannot clear, freeze, or replace the classic grid.

## Envelope

Messages use a 7-bit APC string:

```text
APC  = ESC _
ST   = ESC \
body = printable ASCII, at most 4096 bytes
```

The body starts with the case-sensitive namespace `Prismattyc;`. xterm
documents unimplemented APC functions as ignored, and Kitty uses a distinct
`G`-prefixed APC namespace for its graphics protocol. Prismattyc does not reuse
Kitty's namespace.

References:

- [XTerm Control Sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf)
- [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)

## Query and reply

Control data is semicolon-separated `key=value` ASCII. Unknown keys are
ignored; duplicate required keys, invalid values, or an oversized body make the
whole message invalid.

```text
# Application -> Prismattyc
ESC _ Prismattyc;cap;q;id=7;max=0.4 ESC \

# Prismattyc -> application (native sessions on, Linux)
ESC _ Prismattyc;cap;r;id=7;v=0.4;features=native.grid.v1,native.images.v1;native.offer=...;native.generation=... ESC \
```

| Message | Field | Meaning |
|---------|-------|---------|
| Both | `id` | Non-zero decimal `u32`; the reply echoes the query id |
| Query | `max` | Highest protocol version the application understands |
| Reply | `v` | Version selected by the host |
| Reply | `features` | Comma-separated feature tokens |

Version syntax is `major.minor`. Only a pane with native sessions on answers,
and only with the native tokens its platform supports. Every other pane stays
silent, so the application times out and keeps its classic output. The native
offer fields and what follows them are defined by Lunatui's
[grid contract](https://github.com/moonbase2090/lunatui/blob/main/docs/native-protocol.md).

Applications should use a bounded timeout appropriate to their transport and
must not block their main UI indefinitely. Repeated probing must be
rate-limited.

**PTY-attached clients** must disable canonical mode and echo (`ICANON` /
`ECHO`) **before** sending the capability query. Host APC replies have no
trailing newline, so a line-disciplined stdin never delivers them and `ECHO`
reprints the reply as visible junk. Piped CI harnesses have no line discipline
and do not need it.

## Multiplexers and remote hops

Only the direct application -> Prismattyc path is supported. tmux documents an
explicit DCS wrapper for passing otherwise unknown escape sequences through to
the outer terminal; arbitrary APC forwarding must not be assumed.

- [tmux pass-through sequence](https://github.com/tmux/tmux/wiki/FAQ#what-is-the-passthrough-escape-sequence-and-how-do-i-use-it)

No reply through an unaware multiplexer means classic-only, not an error.
Prismattyc does not advertise support because `$TERM` or an environment
variable says `prism`. `encode_tmux_passthrough` is the documented DCS helper
for tests; it is not a production-support claim. See the
[transport matrix](capability-transport-matrix.md).

## Parser and security requirements

- Accept only printable ASCII within the APC body.
- Cap the body at 4096 bytes before allocating based on its contents.
- Reject zero request ids, invalid versions, duplicate required fields and
  unterminated strings.
- Ignore unknown keys for forward compatibility.
- Never include secrets or filesystem paths in capability data.
- Do not emit a diagnostic into the child PTY stream for malformed input.

## Rust types

`prismattyc-protocol` owns the dependency-light pieces: `ProtocolVersion`,
`RequestId`, `CapabilityQuery`, the `ApcCollector` that the emulator runs
beside its VT parser, `decode_body` for the query, the graphics APC parser and
the native grid wire types. The protocol crate does not depend on the screen,
renderer, mux or PTY. Its decoder tests cover malformed, fragmented,
oversized and unknown-field input.
