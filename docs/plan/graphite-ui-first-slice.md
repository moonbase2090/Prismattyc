# Graphite UI: first slice after 0.3.0

**Status:** scoping for MB2090. Docs only. It proposes child tickets. It does not file them and does not change code.

**Verified against `origin/main` at `6415bda`.** Each claim below names the PR or file it came from.

## 1. What already shipped

The design brief's seven items are done. Their sub-issues #104 to #114 are all closed, and #114 flipped Graphite to the default for 0.3.0 (`docs/design/graphite-ui-brief.md`, "Decisions"). Tracking issue #103 is still open only because P8 below is not merged.

## 2. The crate extraction (`docs/design/graphite-crate-plan.md`, PR #182, merged)

The plan's host PRs P1 to P7 are merged:

| Plan row | PR | State |
| --- | --- | --- |
| P1 git pin | #221 | merged |
| P2 color helpers | #222 | merged |
| P3 token derivation | #224 | merged |
| P4 tabs-bar layout and hits | #225 | merged |
| P5 rail and side-column geometry | #226 | merged |
| P6 sidebar layout and hits | #227 | merged |
| P7 pane model and light-cycle path | #243 | merged |
| P8 legend and theme-picker geometry | none | **not started** |

The graphite repo (`github.com/moonbase2090/graphite`, public) has the core modules that P8 needs: `legend::legend_keys` (`src/legend.rs`), `picker::theme_picker_layout`, `theme_picker_visible_rows` and `theme_picker_hit` (`src/picker.rs`), and `translucency::translucency_roles` and `alpha_policy` (`src/translucency.rs`). Those are plan rows G8 and G9.

On `origin/main`, `graphite_overlays.rs` has no `graphite_core` calls. Its host copies are `legend_metrics`, `legend_run_width` and `legend_keys` (around lines 1023 to 1076), and `theme_picker_layout`, `theme_picker_visible_rows`, `theme_picker_hit` and `theme_picker` (around lines 556 to 650). P8 replaces those bodies with core calls.

## 3. Proposed child tickets

Each ticket is one reviewable PR. Trunk tickets are labelled `trunk`, and each needs an independent review.

### T1. P8a: legend packing calls `graphite-core`
- **Class:** trunk (`prismattyc-host`, the plan's rule).
- **Change:** `legend_keys` builds runs and calls `legend::legend_keys` for packing. Painting and `KeyMap` label resolution stay in the host. The host's `legend_metrics` and run-width helpers are deleted in the same PR.
- **Tests that stay green:** `legend_keys_draw_focus_color_and_bar_keycaps`, `legend_keys_drop_a_shortcut_that_does_not_fit`, `legend_notice_does_not_draw_keycaps`.
- **Proof:** the test output, and a pixel check of the legend at 1x and 2x against the base commit. The plan says P-PRs preserve behavior, so no visual diff is allowed.

### T2. P8b: theme-picker frame calls `graphite-core`
- **Class:** trunk.
- **Change:** `theme_picker_layout`, `theme_picker_visible_rows` and `theme_picker_hit` delegate to `picker::*`. Theme names, the catalog and `theme_picker` paint stay in the host.
- **Tests that stay green:** `theme_picker_geometry_is_fixed_and_scrolled_hits_are_absolute`, `palette_fixed_layout_does_not_follow_match_count_and_filter_hits_align`.
- **Proof:** the test output, and a before and after capture of the picker at its fixed 760x460 size, scrolled to the last row.

### T3. P8c: translucency role table is consumed by the painters
- **Class:** leaf (a documented constant in the painters; the plan says "no behavior change").
- **Change:** painters read `alpha_policy` for opaque roles and keep existing colors. No blend function moves into the core.
- **Test:** a host test that lists the brief's opaque set (text, cursor, status dots, "needs you" badge, active tab chip, command field, focus ring) and checks each maps to opaque.
- **Proof:** test output.

### T4. Close #103 after T1 to T3 merge
- **Class:** docs and tracking only. No PR. Close #103 with a link to the merged P8 PRs, and record the outcome on the board. This is the last item the Graphite epic needs.

### Not proposed here
- **T5 (graphite-tui snapshot tests):** the graphite repo has `graphite-tui` (#31 merged), and its tests cover the widgets. I did not verify whether plan rows T2 to T5 are complete, so I'm not proposing them. Check before scoping.
- **Scorecard S1 and lunatui T6:** owned by those repos and out of scope for Prismattyc.
- **Publishing `graphite-core` to crates.io:** the plan keeps this as a separate sign-off (Q4). Not in this slice.

## 4. Open risks for this slice

- **CRAP and mutation gates:** deleting host bodies in P8 removes code those gates measured. The plan says the graphite repo's CI is the gate from G0 onward. Confirm in T1's review.
- **Pin bump:** P8 may need a newer `graphite-core` revision than the one pinned in `Cargo.lock` today. The pin changes in the same PR as the call, per the plan's §5.2.
- **Pixel parity:** the plan says these PRs do not change pixels. A visual diff on the legend or the picker blocks the PR.

## 5. Questions for MB2090

1. Confirm the split: T1 and T2 as separate PRs, T3 as its own leaf PR.
2. Should T1 to T3 wait until the pinned `graphite-core` is bumped, or should T1 do the bump itself?
