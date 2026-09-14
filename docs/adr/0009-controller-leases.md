# ADR-0009 — Connection-bound pane controller leases

**Status:** Accepted
**Date:** 2026-08-11
**Depends on:** ADR-0007; ADR-0008

## Context

Observers and controllers share a same-user local control socket, but same UID
does not mean same client. A wire-supplied integer must not let one connection
impersonate another pane's controller, revive a disconnected identity, or clear
somebody else's leases. Input must also never report success before bytes have a
real, lossless route to the pane PTY.

## Decision

Controller leases remain exclusive and per pane. `acquire` claims a free lease,
`release` requires the holder, and `takeover` is the explicit operation that may
replace another holder. Disconnect clears all leases held by that client.

The control plane maintains a live set of IDs minted by `register_client`.
Unknown, never-registered, and disconnected IDs are `stale_id`. Each Unix socket
connection may register once, and every client-qualified request must carry that
connection's exact ID. Re-registration and cross-connection impersonation are
rejected before mutation.

`write_pane` bounds the payload, then checks live identity, pane existence,
and controller authority. A clean pane with no controller accepts a
lease-free write. Another client's controller causes `not_controller`.
An unleased dirty pane causes `input_dirty`. Accepted bytes enter the live
PTY input queue. `write_queued` is a queue receipt, not an application ACK.
A missing route causes `input_route_unavailable`.

`pane_write` adds intentional collaboration text for one pane. It checks
the expected child PID and refuses dirty input even from the lease holder.
It does not acquire leases or copy text through sync-input. It reports
partial queueing. See the [pane-write protocol](../pane-write-protocol.md).

## Consequences

- At most one client holds a controller lease per pane. Another same-UID
  connection cannot borrow that lease by copying its numeric ID.
- With no controller, clean input accepts serialized lease-free writes.
- IDs never become valid again after disconnect.
- Socket drop cleanup is deterministic and does not depend on a cooperative
  client request.
- Input receipts must report queueing accurately, including partial writes.

## Out of scope

- Remote authentication or multi-machine transport
- Visible controller/takeover chrome
- Detach/reattach lifetime and reconnect identity continuity
