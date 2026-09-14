# Phase 2B detach/reattach durability proof

Run the deterministic harness from the repository root:

```sh
./scripts/test-phase2b-detach.sh
```

The harness builds and launches the landed `prismattyc-mux-server`; it does not
introduce a second process-lifetime owner. It then:

1. waits for the private local socket;
2. attaches a first client and writes two run-unique markers;
3. lets that client disconnect;
4. proves the same server PID and PTY child PID are alive with `kill -0`;
5. attaches a fresh client and waits for both markers;
6. proves pane ID, child PID, server-owned content, and `child_alive=true` are
   unchanged without a respawn.

Each run retains a small evidence bundle under
`e2e/artifacts/phase2b-detach/<UTC-run-id>/`:

| File | Evidence |
|------|----------|
| `first-attach.json` | Initial pane ID, child PID/alive state, and first client frame |
| `reattach.json` | Fresh-client frame with both markers and the same pane/child identity |
| `summary.txt` | Machine-readable PASS and lifetime/topology assertions |
| `server.stdout` / `server.stderr` | Server launch diagnostics |

`child_pid` is diagnostic metadata only. Stable control targets remain opaque
`PaneId`/`WindowId`/`SessionId` values; PIDs are never accepted as protocol
targets.

This proves detach durability for a live local server. Killing or crashing the
server is **not** detach, and makes no cold-resurrection, remote transport,
or Phase 3 rich-content claim. The automated run is implementation evidence,
not an external A-6 cohort completion.
