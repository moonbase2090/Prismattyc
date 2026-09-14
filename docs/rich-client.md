# Authoring a Prismattyc rich client

`prismattyc-rich-client` is the public app-side session library. An independent
application needs only it and `prismattyc-protocol`; it does not import host, mux,
renderer, or compositor types. The runtime-validated
[`surface` example](../crates/prismattyc-rich-client/examples/surface.rs) emits a
minimal protocol 0.3 workspace, collection, semantic document, status badge,
clean drop, and complete classic fallback.

This guide is first-party integration documentation. Validating the example
does not count as a PRD section 5.6 Independent integration: that gate still
requires eligible non-core authors.

## Choose the frozen query

- Existing cell-rect apps keep `encode_default_query()` and
  `await_session(NEGOTIATE_TIMEOUT)`. That protocol 0.2 query and all 0.1/0.2
  reply bytes remain frozen.
- Workspace apps use `encode_surface_query()` and
  `await_surface_session(NEGOTIATE_TIMEOUT)`. The query asks for at most
  protocol 0.3.

If stdin is a TTY, retain the guard returned by `enter_raw_stdin()` while
negotiating and running. An APC reply has no newline, so canonical input would
otherwise hold it. Write and flush the query before waiting.

Treat every `Session::Classic` reason as normal fallback: timeout, child exit,
malformed reply, unsupported reply, or a reply without the required workspace
features. Do not emit rich frames before a grant.

## Build one authoritative state model

The application owns task state, text, actions, collection contents, semantic
meaning, and progress facts. Prismattyc owns only bounded validation, transport,
layout, theme resolution, and viewer-local interaction state.

For a workspace session:

1. Require `grant.has_workspace()`; otherwise render the complete classic UI.
2. Build a full keyed tree and call `grant.workspace_snapshot(rows, nodes)`.
   The grant supplies the generation and next monotonic scene revision.
3. Encode the returned value with `encode_workspace_snapshot()`.
4. Send optional layers only when their capability is present:
   `collection_snapshot`/`collection_append`/`collection_replace`, a bounded
   `SemanticDocument`, and `status_snapshot`.
5. Rebuild those projections from the same app-owned model after every real
   state change. Restore, resize, and reattach are display operations and must
   never start work.

Tree text is authoritative. A status badge, meter, or sparkline decorates a
text node but never replaces its label. Send semantic tones, not RGB or theme
palette values. Indeterminate meters are static; applications do not send a
clock or animation phase.

## Revisions and bounded collections

`RichGrant` is the only writer of app-side shared revisions. Keep one grant for
the negotiated child lifetime:

- `workspace_snapshot` advances `scene_rev` and refreshes action bindings;
- collection snapshots and patches advance each collection independently;
- `status_snapshot` advances a replaceable status revision bound to the
  current workspace scene;
- semantic documents use an application-owned monotonic revision and Unicode
  scalar offsets.

Never invent a revision in response to the host. On collection reject or
resnapshot, call `reset_collection(id)` and send a full snapshot. Append-only
diagnostics keep `replaceable=false`; replaceable task/status rows may coalesce
only through an explicit replace patch. Protocol bounds and advertised limits
are listed in [capability-protocol.md](capability-protocol.md).

## Validate structured input before mutation

Feed every host APC body through `RichGrant::decode_event_mut` (or the
equivalent mutable-session helper). Only `Incoming::StructuredInput` reaches
the application reducer. The validator requires:

- the current surface generation and scene revision;
- a host-minted viewer that first received `Focus(true)`;
- a strictly increasing request sequence;
- the current node/action binding and, when present, collection revision;
- a separately granted keyboard, pointer, or scroll family.

Focus revocation, pane switch, viewer detach, generation change, removed
nodes, replayed requests, and stale revisions make later input inert. Reserved
host chords never become application actions.

## Semantics, copy, and status

`rich.semantic_text.v1` carries a bounded plain-text document plus roles and a
logical selection. Offsets count Unicode scalar values, not bytes or terminal
cells. When the host requests copy, validate generation, document, revision,
and range before projecting decoration-free text.

`rich.status.v1` currently supports only:

- a text-labelled badge;
- a determinate or static indeterminate meter;
- a sparkline of at most 16 app-reported samples.

Every status item references a text node in the accepted scene. A malformed,
stale, or wrong-node status snapshot drops only the status layer; it must not
blank the workspace or guest grid.

## Teardown and failure behavior

On a clean protocol 0.3 exit, write `encode_workspace_drop(generation)`. A
0.1/0.2 attachment app still detaches all registered region IDs. On a
mid-session rich failure, keep the child and classic transcript usable; do not
enter an alternate screen merely because the rich surface disappeared.

The fallback screen must contain every action and fact needed to finish the
task. Rich-only state is a bug. Keep logs and commands local; the client
library does not upload application data.

## Verify an app

From this repository. `test-phase3-rich.sh` is a frozen regression pin.
It is not a Phase 3 / A-6 gate.

```bash
cargo test --locked -p prismattyc-rich-client --example surface
./scripts/test-phase3-rich.sh
```

For an independent package, declare only `prismattyc-rich-client` and
`prismattyc-protocol`, run the same app under a granting 0.3 host and a terminal
that never replies, then exercise malformed, stale, drop, reattach, resize,
flood, clipboard, and quiet-idle paths. Record any undocumented intervention;
such intervention makes the PRD Independent integration result inconclusive
until the guide/API is revised and retried.
