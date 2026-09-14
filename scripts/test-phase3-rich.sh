#!/usr/bin/env bash
# Deterministic Phase 3 rich harness.
# Pins named rich tests and re-runs Spike Baseline v0 fixtures with the
# experimental flag off and on (phase-0b-spike.md:105).
# Not a PRD §5.6 pass and not the owner's production-rich checkpoint.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export CARGO_TERM_COLOR=never

rich_protocol_tests=(
  "tests::v1_query_max_01_is_byte_identical_to_spike"
  "tests::styled_run_text_percent_escapes_delimiters"
  "tests::styled_run_text_rejects_bad_percent_sequences"
  "tests::fuzz_corpus_adversarial_inputs_do_not_panic"
  "tests::focus_query_reply_and_key_round_trip"
  "tests::focus_key_rejects_empty_and_oversized"
  "tests::surface_query_preserves_frozen_01_and_02_replies"
  "tests::surface_query_advertises_only_implemented_slice_and_bounds"
  "tests::workspace_snapshot_round_trips_and_drop_is_explicit"
  "tests::workspace_rejects_unknown_cycle_and_node_overflow"
  "tests::workspace_geometry_preserves_transcript_and_too_small_falls_back"
  "tests::collection_snapshot_round_trips_and_patches_are_ordered"
  "tests::collection_backpressure_and_replaceable_update"
  "semantics::tests::apply_rejects_older_and_conflicting_same_revision"
  "semantics::tests::project_copy_rejects_foreign_generation_or_document"
  "semantics::tests::wide_characters_use_scalar_offsets_and_survive_the_wire"
  "tests::status_snapshot_rejects_invented_or_unbounded_values"
  "tests::status_snapshot_round_trips_static_primitives_and_drop"
)
rich_host_tests=(
  "rich::tests::grant_required_before_attach"
  "rich::tests::paint_policy_skips_primary_overlays_on_alt"
  "rich::tests::scroll_translates_then_detaches_fully_above"
  "rich::tests::process_rich_chunk_flood_stays_responsive"
  "rich::tests::rich_focus_grant_revoke_and_detach"
  "rich::tests::rich_focus_does_not_auto_grant_and_flag_off_has_no_focus"
  "rich::tests::two_regions_keep_independent_z_and_damage"
  "rich::tests::resize_below_region_hides_rows_past_grid"
  "rich::tests::flood_with_live_region_updates_stays_responsive"
  "mux::tests::flag_off_emulator_does_not_collect_apc"
  "mux::tests::flag_on_emulator_collects_apc"
  "mux::tests::closing_a_rich_pane_drops_its_regions"
  "raster::tests::overlay_paint_clips_to_pane_content_rect"
  "raster::tests::overlay_paint_clips_after_pane_shrink"
  "raster::tests::overlay_paint_clips_partially_scrolled_region"
  "raster::tests::viewport_overlay_paints_above_cell_rect"
  "rich::tests::workspace_is_grant_gated_ordered_and_malformed_drops_only_surface"
  "rich::tests::collection_gap_drops_only_that_collection_and_requests_resnapshot"
  "rich::tests::cached_collection_does_not_overwrite_workspace_tree"
  "rich::tests::semantic_snapshot_copies_location_without_decoration"
  "rich::tests::status_is_scene_bound_static_and_malformed_drops_only_status"
  "theme::tests::status_tones_resolve_through_each_active_theme"
  "tests::idle_control_flow_is_wait_not_poll"
)
rich_tty_tests=(
  "tests::paint_policy_skips_primary_overlays_on_alt"
  "tests::process_rich_chunk_flood_stays_responsive"
  "tests::experimental_on_replies_and_attaches_cell_rect"
  "tests::v1_negotiate_grants_viewport_and_limits"
  "tests::update_mutates_text_without_geometry"
  "tests::viewport_stays_pinned_while_cell_rect_translates"
)
classic_core_tests=(
  "wraps_only_when_the_next_character_arrives"
  "scrolling_is_bounded"
  "erase_line_uses_current_style"
  "cursor_movement_is_clamped_to_the_grid"
)
classic_emu_tests=(
  "parses_text_cursor_motion_and_erasure"
  "parses_basic_sgr_attributes_and_colors"
  "unsupported_sequences_do_not_leak_payload_text"
  "real_pty_captures_child_output"
)
classic_render_tests=(
  "plain_text_preserves_grid_shape"
  "ansi_renderer_emits_style_and_restores_cursor"
)
classic_tty_tests=(
  "zero_sized_synthetic_terminal_gets_a_usable_fallback"
)

require_tests() {
  local package="$1"
  shift
  local listing
  listing="$(cargo test --locked -p "$package" -- --list)"
  local test_name
  for test_name in "$@"; do
    if ! grep -Fq "$test_name: test" <<<"$listing"; then
      echo "phase3-rich: missing required $package test: $test_name" >&2
      exit 2
    fi
  done
}

run_named() {
  local package="$1"
  shift
  local test_name
  for test_name in "$@"; do
    cargo test --locked -p "$package" "$test_name" -- --test-threads=1
  done
}

rich_client_tests=(
  "default_query_bytes_match_protocol"
  "timeout_and_missing_body_are_classic"
  "malformed_reply_is_classic"
  "unsupported_reply_is_classic"
  "events_drop_in_classic_and_accept_in_rich"
  "detach_encodes_every_id_and_drop_is_safe"
  "reply_must_echo_query_id"
  "spike_grant_does_not_accept_focus_events"
  "child_exit_reason_is_distinct_from_timeout"
  "negotiate_skips_stale_and_unrelated_apc"
  "surface_grant_builds_monotonic_workspace_snapshots"
  "negotiation_rejects_reply_newer_than_requested_max"
  "negotiation_stops_at_reply_terminator_without_eating_input"
)
rich_render_tests=(
  "workspace_layout_adapts_without_changing_logical_ids"
  "workspace_layout_clips_chrome_and_rejects_unsatisfiable_tree"
  "workspace_layout_returns_none_below_frozen_geometry"
  "overlay_collection_cache_paints_reattach_window"
  "indeterminate_meter_has_no_time_or_animation_phase"
  "narrow_badge_keeps_authoritative_label_before_decoration"
  "status_layer_is_static_themed_and_narrow_text_first"
)
rich_mux_tests=(
  "workspace_snapshot_reserves_rows_and_malformed_tree_drops_surface"
  "styled_workspace_paints_above_guest_and_offsets_cursor"
  "flag_off_json_omits_overlays"
  "semantic_copy_rejects_stale_generation"
  "status_snapshot_survives_mux_layout_without_animation_state"
  "observer_styled_read_does_not_steal_controller_clipboard"
)

require_tests prismattyc-protocol "${rich_protocol_tests[@]}"
require_tests prismattyc-host "${rich_host_tests[@]}"
require_tests prismattyc "${rich_tty_tests[@]}"
require_tests prismattyc-rich-client "${rich_client_tests[@]}"
require_tests prismattyc-render "${rich_render_tests[@]}"
require_tests prismattyc-mux "${rich_mux_tests[@]}"

echo "phase3-rich: named protocol / host / TTY rich proofs"
run_named prismattyc-protocol "${rich_protocol_tests[@]}"
run_named prismattyc-host "${rich_host_tests[@]}"
run_named prismattyc "${rich_tty_tests[@]}"
run_named prismattyc-rich-client "${rich_client_tests[@]}"
run_named prismattyc-render "${rich_render_tests[@]}"
run_named prismattyc-mux "${rich_mux_tests[@]}"

run_classic() {
  local label="$1"
  echo "phase3-rich: classic Spike Baseline v0 fixtures ($label)"
  run_named prismattyc-core "${classic_core_tests[@]}"
  run_named prismattyc-emulator "${classic_emu_tests[@]}"
  run_named prismattyc-render "${classic_render_tests[@]}"
  run_named prismattyc "${classic_tty_tests[@]}"
}

unset PRISMATTYC_EXPERIMENTAL_RICH || true
run_classic "flag off"

PRISMATTYC_EXPERIMENTAL_RICH=1 run_classic "flag on"

echo "phase3-rich: PASS (named rich + classic fixtures flag off/on)"
echo "phase3-rich: not a PRD §5.6 pass; A-6 remains deferred"
