# Phase 2A proof harness

packages Prismattyc's deterministic, display-free multiplexer acceptance
probes behind one named command:

```bash
./scripts/test-phase2a.sh
```

The script first verifies that every required Rust test still exists, then runs
the eight proofs serially. It honors normal Cargo settings such as
`CARGO_TARGET_DIR`, `CARGO_INCREMENTAL`, `CARGO_BUILD_JOBS`, and `RUSTFLAGS`.

## Proof matrix

| Contract | Deterministic evidence |
|---|---|
| Live multi-PTY composition | Three `/bin/sh` PTYs receive distinct markers through focused routing; each other emulator is checked for absence of the marker. |
| Layout conservation | The three live pane rectangles cover the full 80×24 grid with no overlap. |
| Per-pane resize | Distinct emulator dimensions and live `stty size` output prove SIGWINCH/winsize reaches both focused and unfocused PTYs. |
| Offscreen composition | Painting a pane at an offset changes only its assigned pixel region and cannot clear its neighbor. |
| Stale IDs | Destroyed pane IDs plus never-registered and post-disconnect client IDs are rejected as `stale_id` without reviving topology or authority. |
| Wrong writer | A second socket cannot spoof the controller's `client_id`; its own observer write remains `not_controller`. An authorized controller receives `input_route_unavailable` until a real PTY sink exists. Re-registration on one connection is rejected. |
| Resync | A replay gap revokes event eligibility until the connection takes a fresh snapshot. |
| Backpressure | A client that stops reading large snapshot responses does not prevent an independent client from receiving a bounded response. |
| Stale socket | Bind replaces a same-uid leftover with no listener; live and foreign paths are left untouched. |
| Session verbs | `create_session` / `switch_session` append ordered events; duplicate/empty names and stale ids are rejected. |

## Trust boundary hardened by the harness

`ControlPlane` now tracks live registered clients. Lease, write, disconnect, and
client-qualified split/close requests reject unknown or disconnected IDs. The
Unix socket additionally binds the claimed ID to the connection that registered
it, preventing another same-UID local connection from impersonating an active
controller.

## Relationship to other gates

- Run `cargo test --workspace --locked` and strict workspace Clippy for the full
  source gate; this harness is a named acceptance subset, not a replacement.
- Run `./scripts/termwright-e2e.sh` for nested/shared VT regressions.
- Use real `prismattyc-host` OS-window screenshots for chrome and visual dogfood.
  Termwright is intentionally not the windowed compositor harness.
- Detach/reattach lifetime is Phase 2B and is not claimed by this 2A harness.
