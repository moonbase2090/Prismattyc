#!/usr/bin/env bash
# Deterministic Phase 3 rich harness.
# Pins named rich tests and re-runs Spike Baseline v0 fixtures with the
# experimental flag off and on.
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

rich_render_tests=(
  "workspace_layout_adapts_without_changing_logical_ids"
  "workspace_layout_clips_chrome_and_rejects_unsatisfiable_tree"
  "workspace_layout_returns_none_below_frozen_geometry"
  "overlay_collection_cache_paints_reattach_window"
  "indeterminate_meter_has_no_time_or_animation_phase"
  "narrow_badge_keeps_authoritative_label_before_decoration"
  "status_layer_is_static_themed_and_narrow_text_first"
)
require_tests prismattyc-protocol "${rich_protocol_tests[@]}"
require_tests prismattyc-render "${rich_render_tests[@]}"

echo "phase3-rich: named protocol rich proofs"
run_named prismattyc-protocol "${rich_protocol_tests[@]}"
run_named prismattyc-render "${rich_render_tests[@]}"

echo "phase3-rich: classic Spike Baseline v0 fixtures"
run_named prismattyc-core "${classic_core_tests[@]}"
run_named prismattyc-emulator "${classic_emu_tests[@]}"
run_named prismattyc-render "${classic_render_tests[@]}"
run_named prismattyc "${classic_tty_tests[@]}"

echo "phase3-rich: PASS (named rich + classic fixtures)"
echo "phase3-rich: not a PRD §5.6 pass; A-6 remains deferred"
