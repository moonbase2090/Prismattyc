#!/usr/bin/env bash
# Deterministic, display-free Phase 2B server/attach architecture proofs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export CARGO_TERM_COLOR=never

server_tests=(
  "control::tests::phase2b_live_server_client_disconnect_preserves_pty_and_emulator"
  "control::tests::phase2b_live_server_topology_mutations_keep_runtime_in_lockstep"
  "control::tests::phase2b_idle_attach_survives_bounded_read_timeouts"
  "control::tests::phase2b_output_activity_is_emitted_and_coalesced"
  "control::tests::phase2b_shutdown_server_is_accepted_then_server_exits_cleanly"
  "control::tests::phase2b_live_runtime_preserves_pty_across_move_pane"
)

listing="$(cargo test --locked -p prismattyc-mux phase2b_ -- --list)"
for test_name in "${server_tests[@]}"; do
  if ! grep -Fq "$test_name: test" <<<"$listing"; then
    echo "phase2b-server: missing required prismattyc-mux test: $test_name" >&2
    exit 2
  fi
done

echo "phase2b-server: ownership, detach lifetime, topology lockstep, and idle attach"
cargo test --locked -p prismattyc-mux phase2b_ -- --test-threads=1

echo "phase2b-server: PASS (${#server_tests[@]} server/attach proofs)"
