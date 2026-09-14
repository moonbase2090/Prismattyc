#!/usr/bin/env bash
# Deterministic, display-free Phase 2A mux proof matrix.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export CARGO_TERM_COLOR=never

host_tests=(
  "mux::tests::phase2a_live_multi_pty_wrong_pane_input_isolation"
  "mux::tests::phase2a_resize_applies_distinct_geometry_to_every_pty_and_emulator"
  "raster::tests::phase2a_offscreen_pane_offset_does_not_clear_neighbor"
)
mux_tests=(
  "control::tests::phase2a_close_removes_spawn_metadata_and_unknown_ids_are_stale"
  "control::tests::phase2a_wire_event_gap_revokes_snapshot_until_resync"
  "control::tests::phase2a_slow_observer_response_backpressure_does_not_block_fast_client"
  "control::tests::phase2a_unregistered_and_disconnected_client_ids_are_stale"
  "control::tests::phase2a_socket_client_identity_cannot_be_re_registered_or_spoofed"
  "control::tests::phase2a_bind_replaces_stale_same_uid_socket_leftover"
  "control::tests::phase2a_create_and_switch_session_are_ordered_events"
)

require_tests() {
  local package="$1"
  shift
  local listing
  listing="$(cargo test --locked -p "$package" phase2a_ -- --list)"
  local test_name
  for test_name in "$@"; do
    if ! grep -Fq "$test_name: test" <<<"$listing"; then
      echo "phase2a: missing required $package test: $test_name" >&2
      exit 2
    fi
  done
}

require_tests prismattyc-host "${host_tests[@]}"
require_tests prismattyc-mux "${mux_tests[@]}"

echo "phase2a: prismattyc-host live PTY, geometry, and offscreen proofs"
cargo test --locked -p prismattyc-host phase2a_ -- --test-threads=1

echo "phase2a: control stale-ID, wrong-writer, resync, and fairness proofs"
cargo test --locked -p prismattyc-mux phase2a_ -- --test-threads=1

echo "phase2a: PASS (${#host_tests[@]} host + ${#mux_tests[@]} control proofs)"
