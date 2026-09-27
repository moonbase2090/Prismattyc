# Remote Spaces over SSH

Status: implementation contract for issue #24. The scope is approved. The interfaces below are proposed and are not implemented by this change.

## User flow

Configure an SSH destination using an existing SSH host alias. Refresh that destination in the Mac host to list its running Spaces. Select a Space once to attach its active session, with its other sessions available through remote Space navigation.

The remote daemon retains session, process, pane and mailbox ownership. Disconnecting the Mac view leaves those processes running. The host renders the remote terminal stream with the existing tile presenter.

## Current behavior and evidence

At main `79a366a`, `SpaceRail` stores local names and the host refreshes it from `spaces_dir()`. That directory resolves through the local data-home configuration. An SSH process in a pane does not supply a catalog to that rail.

`cmd_space_attach` already selects the requested or active saved session and calls `exec_attach_session` with a Space name. That function can route to an existing host or execute `pmux-attach --session ... --space ...`. The report that a second session attach was necessary is still unconfirmed. Record the exact command, transcript, host version, attach version, live daemon version, PTY allocation, and routing environment before attributing that symptom to a defect.

Source locations:

- `crates/prismattyc-host/src/space_rail.rs`: rail state, layout and hit testing.
- `crates/prismattyc-host/src/main.rs`: rail refresh and Space opening.
- `crates/prismattyc-mux/src/layout_file.rs`: saved Space identity, tab order and local storage.
- `crates/prismattyc-mux/src/bin/pmux.rs`: catalog and attach CLI boundaries.
- `crates/prismattyc-mux/src/host_register.rs`: host routing and explicit PTY fallback.

## Ownership and types

The following Rust sketches describe the contract, not an existing API:

```rust
enum SpaceKey {
    Local(LocalSpaceId),
    Remote { destination: DestinationId, space: RemoteSpaceId },
}

struct SshDestination {
    id: DestinationId,
    label: String,
    ssh_alias: SshAlias,
}

struct RemoteSpace {
    id: RemoteSpaceId,
    name: String,
    sessions: Vec<RemoteSession>,
    active_session: RemoteSessionId,
}

enum CatalogState {
    Disconnected,
    Loading { generation: u64 },
    Ready { generation: u64, spaces: Vec<RemoteSpace> },
    Failed { generation: u64, message: String },
}
```

Parse aliases, identifiers and protocol versions at the configuration and SSH boundaries. Destination IDs namespace remote identifiers so equal Space names on two machines cannot collide. Rail rendering uses display labels; routing uses typed identities. Local rename/delete commands must never receive a remote key.

Legacy saved files can lack stable IDs. The remote catalog must explicitly resolve that case before returning an attachable identity. Do not invent a local UUID that pretends to identify a remote Space. Catalog reads must not launch sessions or replay saved commands.

## Transport contract

Add a versioned machine-readable catalog command to the remote CLI. Its output joins saved Space membership with the daemon's current sessions, so a saved file alone cannot be advertised as a running Space. Reject unsupported versions, malformed records and oversized responses. Remote names are display data, never shell fragments.

Use two SSH channels with separate lifetimes:

1. A bounded, non-PTY catalog request on explicit connect or refresh. The host performs I/O outside its event loop and applies the response only if its destination and generation still match.
2. A PTY channel owned by the attached view. One remote attach operation selects the Space and active session without spawning duplicate sessions or replaying startup commands. Verify whether the existing attach command can meet that rule before choosing the final endpoint.

Use the system SSH configuration and host-key verification. Credentials remain with SSH. Catalog authentication failures become visible connection errors; they cannot block painting while waiting for an invisible password prompt. Define the visible authentication flow before UI integration. Do not silently accept host keys, copy SSH credentials, install remote software, or restart a daemon.

Pass destinations as process arguments. Use a fixed remote command and encode request data through a framed or serialized input contract. Do not concatenate Space names into remote shell command text. Requests have size limits, timeouts and cancellation. A stale response cannot replace a newer catalog.

## Alternatives considered

| Design | Behavior | Decision |
| --- | --- | --- |
| Forward the remote daemon socket into the local host | Reuses daemon operations, but exposes local socket, file and host-routing assumptions across machines | Defer until the protocol has explicit remote ownership and transport support |
| Copy remote Space files locally | Reuses the local rail directly, but misrepresents session ownership and risks rename/delete operations against copied state | Reject |
| SSH catalog plus PTY attach | Keeps catalog ownership remote and reuses terminal presentation with separate control and display lifetimes | Implement this approach |

## Module boundaries

- Shared mux types own the catalog schema and validated destination/Space identities.
- The remote CLI builds the catalog from saved metadata and a daemon snapshot. It owns selection of the live active session.
- A host SSH worker owns subprocesses, deadlines, cancellation and bounded output. It posts results to the host event loop.
- Rail state owns local and remote keys, connection status and selection. Existing geometry and painting consume labels and state changes.
- The attached view owns its local PTY and SSH child. Detach and window close terminate the view transport, not the remote daemon.

Do not generalize the local daemon protocol merely to implement the catalog. Add only the data required for discovery and one-step attach.

## Failure and performance rules

- No SSH command, catalog parse or filesystem probe runs in paint code.
- Disconnected destinations do no recurring network or decorative GPU work.
- Equal catalog snapshots do not cause redraw. A changed connection status or selection damages only the affected chrome.
- Duplicate connect clicks produce one active request per destination. Reconnect replaces the prior transport after cancellation and reaping.
- A stale selected Space produces a visible error. Do not attach an unrelated session by name fallback.
- If no sessions remain live, report the condition instead of restoring saved commands implicitly.
- PTY resize reaches the remote attach process. An EOF or SSH failure leaves local input and terminal modes usable.

## Implementation sequence

- [ ] Establish catalog schema, typed identities and explicit destination configuration.
- [ ] Implement a read-only remote catalog endpoint using a live daemon snapshot.
- [ ] Implement bounded SSH catalog requests with cancellation and error results.
- [ ] Present remote destinations and Spaces in the rail with explicit connection states.
- [ ] Implement one-step PTY attach with session selection and lifecycle cleanup.
- [ ] Exercise isolated SSH catalog, attach, resize, disconnect, reconnect and stale-selection cases.
- [ ] Run native Mac acceptance for discovery and one-step attachment.

Before rail implementation, compare runnable UI sketches for destination grouping and connection states using the current Space rail geometry. Keep the resulting UI decision with this PR.

## Acceptance evidence

Use isolated daemons and test-only SSH accounts for automated reproduction. Do not alter the live development Space or restart its daemon. A native Mac run must prove that the remote catalog appears and a single selection displays the active remote session. Record the other sessions' navigation behavior, resize, detach and reconnect.

Inspect actual displayed prompts and recovery after authentication failure, missing remote CLI, empty catalog and lost connection. Retain transcript and native capture evidence. The separate terminal-database PR does not establish these behaviors. Do not label the historical session-follow symptom fixed until its failure has been reproduced and the same reproduction passes.
