# Capability discovery protocol

How applications discover Prismattyc's opt-in rich features without changing the
classic terminal path.

**Status:** Phase 0 design cut. The capability query/reply envelope and
semantic identifiers below are stable enough to implement. Rich rendering
commands are deliberately out of scope. Protocol `0.3` capability families,
limits, and compatibility fixtures are frozen separately by
[rich surfaces](rich-surface.md); this document remains the
binding envelope and `0.1`/`0.2` compatibility contract.

Related: [architecture.md](architecture.md),
[hybrid-rendering.md](hybrid-rendering.md), and `prismattyc-protocol` in
[workspace.md](workspace.md).

## Invariants

1. Classic VT/xterm behavior is always available and never gated on Prismattyc.
2. An application must receive an affirmative capability reply before emitting
   any Prismattyc-specific rich command.
3. No reply, a malformed reply, or a timeout means classic-only for that
   session.
4. Capability parsing is bounded and must never stall PTY processing.
5. Rich-protocol failure cannot clear, freeze, or replace the classic grid.

## Envelope decision

The first cut uses a 7-bit APC string:

```text
APC  = ESC _
ST   = ESC \
body = printable ASCII, at most 4096 bytes
```

The body starts with the case-sensitive namespace `Prismattyc;`. APC is a good fit
for an application-to-terminal extension: xterm documents unimplemented APC
functions as ignored, while Kitty uses a distinct `G`-prefixed APC namespace
for its graphics protocol. Prismattyc does not reuse Kitty's namespace.

References:

- [XTerm Control Sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf)
- [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)

## Query and reply

Control data is semicolon-separated `key=value` ASCII. Unknown keys are
ignored; duplicate required keys, invalid values, or an oversized body make the
whole message invalid.

```text
# Application -> Prismattyc
ESC _ Prismattyc;cap;q;id=7;max=0.1 ESC \

# Prismattyc -> application
ESC _ Prismattyc;cap;r;id=7;v=0.1;features=style,canvas ESC \
```

Required fields:

| Message | Field | Meaning |
|---------|-------|---------|
| Both | `id` | Non-zero decimal `u32`; reply must echo the query id |
| Query | `max` | Highest protocol version the application understands |
| Reply | `v` | Version selected by the host, not greater than `max` |
| Reply | `features` | Comma-separated advertised feature identifiers |

Version syntax is `major.minor`. A major mismatch produces no successful
reply. Minor versions are additive: the host selects the highest compatible
minor version it implements.

Applications should use a bounded timeout appropriate to their transport and
must not block their main UI indefinitely. They may cache a successful reply
for the lifetime of the terminal session. Retry policy is an application
choice; repeated probing must be rate-limited.

**PTY-attached clients** must disable canonical mode and echo (`ICANON` /
`ECHO`) **before** sending the capability query. Host APC replies have no
trailing newline, so a line-disciplined stdin never delivers them and `ECHO`
reprints the reply as visible junk. `prismattyc-rich-client::enter_raw_stdin`
does this when stdin is a TTY. Piped CI
harnesses have no line discipline and do not need it.

## Initial feature registry

| Identifier | Meaning |
|------------|---------|
| `markup` | Declarative markup regions |
| `style` | Theme/style tokens beyond classic SGR |
| `animation` | Host-driven property animation |
| `canvas` | Canvas primitives |
| `hybrid.attach.cell_rect` | Rich content anchored to a cell rectangle |
| `hybrid.overlay.viewport` | Viewport-local overlay/HUD |
| `input.rich_focus` | Explicit keyboard-focus handoff to a rich target (0.2, `--experimental-rich`; host chord grants, never automatic) |

Unknown identifiers are ignored by older applications. Advertising a feature
means the host can safely parse that feature's messages; it does not grant a
child process additional OS permissions.

Future numeric limits use additional reply keys rather than overloaded feature
names, for example `limit.regions=64`. No limit key is frozen in this cut.

## Implemented protocol 0.3 slice

rich surfaces freezes the complete 0.3 registry and eventual canonical capability
fixture. Implementations advertise features incrementally: registry membership
does not imply a grant. through implement and advertise this
bounded subset:

| Identifier | Implemented behavior |
|---|---|
| `hybrid.reserve.rows` | One bounded top dock; the host subtracts its rows before resizing the guest PTY |
| `rich.tree.v1` | One full keyed-tree snapshot with 128-node and depth-16 validation |
| `rich.collection.v1` | Revisioned task and diagnostic collections with explicit append/replace semantics |
| `input.rich_keyboard.v1` | Consent-gated structured keys bound to viewer, generation, scene, node, and action |
| `input.rich_pointer.v1` | Consent-gated press/move/release activation with cancellation |
| `input.rich_scroll.v1` | Viewer-bound Runbook collection scroll and viewport requests |
| `rich.semantic_text.v1` | Logical roles, ranges, deterministic plain-text projection, and copy requests |
| `rich.status.v1` | Static themed badge, determinate/indeterminate meter, and bounded sparkline layer |

The reply also retains `hybrid.attach.cell_rect`,
`hybrid.overlay.viewport`, and `input.rich_focus`. It advertises the rich surfaces
numeric bounds so independently granted families cannot silently expand
resource use. `markup`, `style`, `animation`, and `canvas` remain
unadvertised.

Workspace wire bodies are:

```text
Prismattyc;workspace;snapshot;generation=G;rev=R;min=5;preferred=P;max=M;nodes=...
Prismattyc;workspace;drop;generation=G
```

Each node is a nine-field record separated by `/`: `id,parent,kind,min,preferred,fill,show_min_cols,show_max_cols,text`.
Text is printable ASCII plus percent-escaped newline and grammar delimiters.
The whole APC remains subject to `limit.body=4096`. Unknown kinds, duplicate or
missing ids, cycles, excess depth/node count, invalid row requests, and bad
escaping drop only the workspace. The classic child grid and PTY remain live.

Status is a separate complete layer tied to an accepted workspace scene:

```text
Prismattyc;status;snapshot;generation=G;scene=S;rev=R;items=...
Prismattyc;status;drop;generation=G
```

Each item names an existing text node and one semantic tone. Records encode
only `badge`, `meter`, or `sparkline`; meters carry app-owned units (or the
static `-,-` indeterminate shape), and sparklines retain at most 16 app-owned
samples. Invalid, stale, duplicate-node, over-limit, or non-text-node status
drops only the status layer. The authoritative tree text remains visible.
Applications never send RGB or animation phases. The windowed host resolves
tones through its active Prismattyc theme; mux attach uses the outer terminal's
active ANSI theme entries. The portable token map is neutral = default
foreground, info = ANSI 4, success = ANSI 2, warning = ANSI 3, and danger =
ANSI 1 on both paths.

## Multiplexers and remote hops

The direct application -> Prismattyc path is the only v0 guarantee. tmux documents
an explicit DCS wrapper for passing otherwise unknown escape sequences through
to the outer terminal; arbitrary APC forwarding must not be assumed.

- [tmux pass-through sequence](https://github.com/tmux/tmux/wiki/FAQ#what-is-the-passthrough-escape-sequence-and-how-do-i-use-it)

Therefore:

- No reply through an unaware multiplexer means classic-only, not an error.
- Prismattyc does not advertise rich support merely because `$TERM` or an
  environment variable says `prism`.
- A future multiplexer integration may proxy the semantic query or define a
  carefully tested pass-through wrapper. That is a separate compatibility cut.
  `encode_tmux_passthrough` is the documented DCS helper for matrix T3;
  it is not a production-support claim.
- SSH changes latency, not the semantic contract; applications still use a
  bounded timeout and graceful fallback.

## Parser and security requirements

- Accept only printable ASCII within the APC body for this message family.
- Cap the body at 4096 bytes before allocating based on its contents.
- Reject zero request ids, invalid versions, duplicate required fields, empty
  feature elements, and unterminated strings.
- Ignore unknown keys and unknown feature identifiers for forward compatibility.
- Never include secrets or filesystem paths in capability data.
- Do not emit a diagnostic into the child PTY stream for malformed input.
- Fuzz the eventual decoder and test fragmented input across arbitrary byte
  boundaries before enabling replies by default.

## Rust types

`prismattyc-protocol` owns dependency-light semantic types:

- `ProtocolVersion`
- `RequestId`
- `CapabilityQuery`
- `CapabilityReply`
- `Feature`

The emulator owns streaming escape recognition but hands a bounded APC body to
`prismattyc-protocol` for decoding. The protocol crate must not depend on the screen,
renderer, mux, PTY, or a rich document model.

The decoder tests cover malformed, fragmented, oversized, and unknown-field
input. Capability discovery does not itself enable rich mode.
