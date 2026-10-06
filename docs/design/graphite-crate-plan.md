# Graphite crate extraction plan

**Status:** for MB2090 sign-off. This document is the plan. It does not change Graphite behavior, and it does not authorize extraction code on its own.

**Baseline:** `origin/main` at `2a4dd0f` (Prismattyc 0.3.0, merge of #178). Counts below are that tree: `crates/prismattyc-host/src/graphite.rs` is 6,338 lines and `graphite_overlays.rs` is 2,586 lines. The assessments on #164 (`c36b502f`) and #169 (`fe19609`) read a smaller pair (about 5,643 and 2,159). The cut list in this plan follows `2a4dd0f`.

**Sources:** issue #164 and its assessment comment, the blind assessment on #169, and the approved lunatui PRD, decision D11 and §5. D11 is the product decision this plan implements: Graphite lives in its own repo, lunatui re-exports it behind an optional feature, and Scorecard's TUI is the first non-Prismattyc consumer.

## 1. Goals

1. Put the renderer-agnostic Graphite look in `graphite-core`: tokens, bar presets, spacing, component models, layout, hit targets, light-cycle path and timing, and the translucency rules.
2. Leave Prismattyc's pixel painters, bundled fonts, palette contents, keymap, sessions, and actions in `prismattyc-host`. After each host PR, Graphite looks and hits the same as `2a4dd0f`.
3. Add `graphite-tui` on top of `graphite-core` and lunatui, once lunatui has theme tokens and a `Widget` trait. Scorecard is the first app that renders those widgets. Lunatui stays look-neutral and re-exports Graphite only behind a default-off feature.
4. Keep every extraction PR small enough to review, with tests that fail before the move and pass after it. Prismattyc host PRs are trunk under `REVIEW_POLICY.md`.
5. Publish nothing to crates.io in this sequence. Consumers pin a git revision until MB2090 opens a separate publish PR.

## 2. Decision this plan adopts

#164 and #169 both found the chrome separable, and both said to wait until after 0.3.0 because there was no second Rust consumer and the release was still a candidate. 0.3.0 has shipped. The approved lunatui PRD names the second consumer (Scorecard's TUI) and the crate split (`graphite-core`, `graphite-tui`).

This plan starts the core extraction now, and starts the TUI crate only when lunatui can host it. Pixel painters stay in Prismattyc for the whole sequence. A ratatui adapter is not part of the sequence. Lunatui is the cell backend. Prismattyc does not take a lunatui dependency here. The in-process Prismattyc sink from lunatui PRD phase 8 is a later plan. Scorecard's TUI, through lunatui, is the consumer this sequence builds toward.

## 3. API boundary

`graphite-core` has no dependency on Prismattyc, lunatui, ratatui, fontdue, wgpu, winit, or serde. Its public items are plain data, pure functions, and one text-measurement trait. `graphite-tui` is the only crate in the Graphite repo that depends on lunatui.

### 3.1 What moves into `graphite-core`

**Color and tokens.** `Rgb`, `Tokens`, `DARK`, `LIGHT`, the four bar presets (Graphite, Harbor, Moss, and Plum/Sand), `bar_color_name`, `step_bar_color`, and `accent` (the 3:1 nudge). Theme derivation moves with them: `uses_brief`, `theme_tokens`, `derive_tokens`, `keep_text_readable`, and the small helpers they need (`toward`, `readable`, `ink_on`). The brief theme ids stay a core constant: `prismattyc`, `prismattyc-dark`, `prismattyc-light`.

The derivation reads a `ThemeSource` the host fills, not `theme::Theme`. The fields it needs from today's `Theme` are: `id`, `variant`, `chrome_bg`, `chrome_fg`, `default_bg`, `default_fg`, `tab_active_bg`, `pane_backdrop`, `pane_border`, `active_badge`, `unseen_badge`, `attention_badge`, and `ansi[4]`.

WCAG helpers that derivation and classic chrome both use move into the core, and the host keeps one-line forwards so there is a single implementation: `raster::mix_rgb`, `raster::relative_luminance`, `raster::contrast_ratio`, `theme::hover_rgb`, and `theme::derived_tab_active_bg`. Classic call sites keep their current function names.

**Design sizes.** The constants in `graphite.rs` that size chrome at scale 1: tabs bar 44, sidebar 256, pane header 28, rail 30, side rail 220 at 18 columns, window pad 8, pane gap 8, pane pad 12, pane radius 8, chip sizes, type sizes, and the sidebar row metrics. Core functions take `scale_milli: u32` (design px × milli / 1000, rounded, same as `ChromeGeom::px`). They do not take `ChromeGeom`. The `graphite: bool` flag stays on the host geometry, because classic layout never calls these functions.

**Text measurement seam.** A `TextMetrics` trait: `width(face, px, text) -> f32`. `ellipsize` moves into the core and calls that trait. `Face` (`Regular`, `SemiBold`, `Mono`) moves with it. The host implements the trait with the existing fontdue cache. Tests implement it with a fake measurer.

**Component models, layout, and hits.**

| Model | Moves | Host still owns |
| --- | --- | --- |
| Tabs bar | `Dot`, `TabText`, `TabSlot`, `BarLayout`, `bar_layout`, `shift_bar`, `bar_hit`, `DropTarget` | Building `TabText` from `TabInfo`. Mapping the core hit onto `mux::StripHit`. Painting (`paint_tabs_bar` and the icons). Inline rename buffer. |
| Spaces rail and side column | `side_rail_px`, `side_header_px`, `side_footer_px`, `side_chip_px`, `side_gap_px`, `side_pad_px`, `side_thumb_px`, `side_thumb_min_px`, `side_cols_for_px`, `rail_chip_width`, `rail_close_width`, `rail_plus_width` | `SpaceRail` data, scroll offset, which spaces exist, and `paint_rail_*` / `paint_side_rail`. |
| Sidebar | `sidebar_layout`, `sidebar_rows_in_view`, `sidebar_header_layout`, `sidebar_hit`, `sidebar_max_scroll`, `sidebar_toggle_rect`, `SidebarHit`, `SIDEBAR_ARRANGE`, `SIDEBAR_ACTIONS` | The tree of sessions, tabs, and panes. `ArrangeTarget` outcomes (spawn shells, zoom, toast). `paint_sidebar*` and the seat icon strip. |
| Pane chrome | `PaneStatus` and `decide`, `pane_handle_rect`, `activity_header_rects` | `PaneHeader` string data supplied by the host. `paint_pane_surface`, `paint_pane_chrome`, `paint_drag_chip`. |
| Light-cycle | Sample path: `RingSweep::for_slot`, `len`, straight and arc sampling. Progress is a fraction of that sample list. | `RingSweep::paint`, `stamp_disc`, and the `BorderUnderlay` budget. Duration stays in host config (`focus_border_animation_ms`, default 280). Reduced motion stays a host flag that asks for the settled ring. |
| Legend | Packing of already-resolved runs: which keycaps fit, which shortcut drops. | Resolving `keybind::KeyMap` to chord labels (`graphite_chord_label` in `main.rs`). Legend paint. |
| Theme picker frame | Fixed 760×460 geometry, row slots, and hit indices that stay absolute while the list scrolls. | Theme names, the theme catalog, and the picker paint. |
| Translucency rules | A table of roles. Opaque: text, cursor, status dots, the "needs you" badge, the active tab chip, the command field, the focus ring. Follow chrome alpha: window ground, pane surfaces, bar backgrounds. | `bar_alpha`, window opacity, blur, and the pixel blend. |

Core hit enums carry indexes, not mux ids. `bar_hit` today returns `StripHit::SpaceMenu`, `Tab { index, close }`, `NewTab`, `Command`, and `EmptyEnd`. The core returns its own enum with those cases. `StripHit::Pane { pane: PaneId }` is a classic strip hit and stays in `mux.rs`. `SidebarHit` has no session id and moves as it is. The host maps core hits to actions in the same place it does now (`main.rs`, `mux.rs`).

`Rect` moves. Host paint code uses the core `Rect` (or a newtype with the same fields) so `mux.rs` can keep storing the last tabs-bar layout for hit testing.

### 3.2 What stays in Prismattyc

- Every `paint_*` function, `draw_text`, `fill_round_rect`, `stroke_round_rect`, `outlined_round_rect`, `paint_dashed_round_rect`, icon drawing, and the glyph cache.
- IBM Plex Sans (`IBMPlexSans-Regular.ttf`, `IBMPlexSans-SemiBold.ttf`) and `assets/fonts/IBMPlexSans-OFL.txt`. The font license stays next to the bytes.
- `fontdue` and the host `TextMetrics` impl.
- Raster pixel packing (`pack_argb`, `unpack_rgb`, `alpha_of`, `raise_alpha`) and the GPU upload path.
- Config enums and serde: `ChromeStyle`, `BarColor`, `LayoutMode`, rail side, opacity keys. The host maps `BarColor` to the core preset. `None` still means "follow the theme."
- `theme::Theme`, the TOML theme files, and `ThemeVariant` on the host. The host maps variant and the `ThemeSource` fields into the core.
- `ChromeGeom` on the host. Call sites that still need "is this Graphite?" keep it. Scale passed into the core is the `scale_milli` field.
- Session, pane, tab, mail, keymap, and command-palette contents. `TabInfo`, `PaneId`, `ArrangeTarget`, and `PresetOutcome` stay in `mux.rs`.
- Overlay content and product chrome: command palette rows, space-menu rows, splash art, walkthrough captions, toasts' strings, the transparency dialog's controls. Their painters in `graphite_overlays.rs` stay. They call core geometry where this plan moves it, and they keep calling host `draw_text`.
- The seat icon strip (`paint_icon_strip`, `IconMark`). Those marks are Prismattyc agents, not the shared look.
- Classic chrome. Classic paint, layout, and hit-testing do not call `graphite.rs` today, and they still do not call `graphite-core` except through the one-line color forwards in §3.1.

`graphite.rs` after the host PRs is painters plus adapters. `graphite_overlays.rs` is product overlays plus the painters that use those adapters.

### 3.3 What `graphite-tui` owns

`graphite-tui` depends on `graphite-core` and lunatui. It implements Rail, Pane header, Chip, Badge, StatusDot, TabStrip, and the shortcut overlay as lunatui widgets. It maps core tokens into lunatui theme tokens, including a 16-color profile. It asks the core for layout by passing lunatui's cell measurer. Rounded corners are glyph choices chosen from the core radius token, as the PRD describes. Translucency is a blend against the cells underneath. Real alpha is the later Prismattyc-native backend, outside this plan.

`graphite-tui` does not depend on Prismattyc, and Prismattyc does not depend on `graphite-tui`. Pixel screenshots are not the acceptance test for these widgets. Styled snapshots are.

### 3.4 Names, license, MSRV

| Piece | Choice |
| --- | --- |
| Repo | `github.com/moonbase2090/graphite` |
| Crates | `graphite-core`, then `graphite-tui` when its PR opens |
| License | MPL-2.0, same as Prismattyc and the lunatui PRD |
| `graphite-core` | Edition 2021, `rust-version = "1.85"`, `publish = false` |
| `graphite-tui` | Matches the lunatui crate it depends on (PRD recommendation: edition 2024, MSRV 1.88) |
| Prismattyc | Stays edition 2021, MSRV 1.90. A 1.85 core is a legal dependency. |
| Visibility | Public from the first push (G0). |

Prismattyc is a public repo. From P1, `ci.yml` runs `cargo check --workspace --locked` and `release.yml` runs `cargo build --release --locked`. Both fetch `graphite-core` with no credential, so the Graphite repo is public. A private core would break those workflows and every source build.

The core stays on Rust 1.85 and edition 2021, enforced by G0's `cargo +1.85 check --locked`, so a direct dependency does not force a newer toolchain. Scorecard's default CLI stays on 1.85 and does not depend on the core. Scorecard's flagged TUI reaches Graphite through lunatui, and that job uses lunatui's MSRV (PRD D7, 1.88). `graphite-tui` is not a Prismattyc dependency, so it can follow lunatui.

lunatui is private as of this plan. T1's CI fetches it with a read credential until lunatui is public, and that job is not part of Prismattyc's public workflows. Scorecard is public. Cargo fetches a git dependency that appears in the default manifest even when its feature is off, so S1 does not add lunatui to Scorecard's default `Cargo.toml`. The flagged TUI job is separate and can authenticate. The default lockfile resolves with no lunatui credential.

On 2026-10-05 the crates.io index showed `graphite` taken and `graphite-core`, `graphite-tui`, `moonbase-graphite`, `graphite-chrome`, `prismattyc-graphite`, and `moonbase-chrome` free (#164). A recheck from this session got HTTP 403 from the crates.io API, so the publish PR has to check again. Nothing in this sequence publishes.

## 4. Dependency direction

Allowed edges while Prismattyc is adopting the core:

```text
graphite-core
    ^
prismattyc-host
    painters, fontdue, IBM Plex, mux, config, themes, actions
```

`lunatui` and `ratatui` are not on that graph. `graphite-core` does not depend on either of them. `prismattyc-host` does not gain a lunatui or ratatui dependency. The new edge is one git dependency, pinned to the `graphite-core` revision that the host PR was tested against.

Allowed edges once `graphite-tui` exists:

```text
graphite-core
    ^            ^
    |            |
prismattyc-host graphite-tui
                     ^
                     |
                 lunatui   feature "graphite" (default off) re-exports graphite-tui
                     ^
                     |
                 scorecard   `scorecard tui`, flag off by default
```

Edges this plan refuses, and that review can check with `cargo tree`:

- `graphite-core` → lunatui, ratatui, any `prismattyc-*` crate, fontdue, wgpu, serde
- `graphite-tui` → ratatui, any `prismattyc-*` crate
- `prismattyc-*` → lunatui, ratatui, `graphite-tui`
- lunatui's default features → `graphite-core` or `graphite-tui`

Ratatui during the migration: it is not a dependency of the core, the TUI crate, Prismattyc, or lunatui's default build. #164 and #169 sketched a ratatui adapter because that was the cell toolkit on the table. D11 replaces that sketch. Scorecard's TUI links lunatui, and lunatui's optional `graphite` feature is how the look is re-exported. A ratatui `Backend` inside lunatui (PRD phase 6, levels L1 and L2) is lunatui's own compatibility work. It is not a Graphite crate and it is not a reason to put ratatui on the Graphite graph.

Order: Prismattyc can finish the core adoption before lunatui has widgets. `graphite-tui` waits for lunatui theme tokens and the `Widget` trait (lunatui PRD PRs 26 and 28). Scorecard's flag waits for `graphite-tui`. The Prismattyc native lunatui backend waits for its own plan.

## 5. PR-by-PR breakdown

One open extraction PR per repo at a time. Each code PR starts with a commit that adds tests failing on the base, then the implementation. The PR body links that commit. Docs-only and CI-only PRs say so and skip the red commit. A PR that moves a host function deletes the host copy in the same PR. Soft size cap: about 600 changed lines outside snapshots and generated fixtures. Split layout from hit-testing if a PR would pass that.

Prismattyc PRs are trunk: they touch `prismattyc-host`. Label `trunk`. Proof is the named tests and the CI run. Independent review is required. A pixel change sends the PR back; these PRs preserve behavior, so they do not add a flag. `chrome_style` stays as 0.3.0 shipped it.

`graphite-core` PRs land in the new repo. Treat them as trunk once Prismattyc depends on the crate. Until then they still need the red test commit, docs on every public item, and a reviewer who is not the author.

The workspace version check in `scripts/check-workspace-version-bumped.sh` applies to Prismattyc PRs that will merge. Each of those PRs bumps the patch and updates `Cargo.toml`, `Cargo.lock`, `README.md`, and `docs/fidelity-matrix-v1.md`, per `CONTRIBUTING.md`. The bump does not publish a release. This sign-off document does not bump the version.

A behavior test that the core also needs is copied, not moved. The host test stays green as the integration check. The core copy pins the same decision against the core API and a fake measurer.

After P1, a token or layout change is a `graphite-core` PR first. It merges after a reviewer who is not the author reviews it. The host PR then bumps the pin and carries the existing pixel or hit tests. Before P1, the same review happens in the Graphite repo, and Prismattyc does not pin an unreviewed revision.

Prismattyc's `mutants-nightly.yml`, the CRAP gate, and `scorecard.yml` measure this workspace. After a P PR deletes a host body, those gates no longer see that logic. From the G PR that added it, the Graphite repo's CI is the gate. G0 does not port mutants or CRAP.

### 5.0 Lunatui PRD phase 5

The approved PRD already lists phase 5 as PRs 36–40. This plan is the breakdown of that phase for Prismattyc and the Graphite repo. Where the two differ, this table says which one governs. Two seats follow this table so they do not build the same chrome twice.

| PRD row | PRD scope | This plan | Governs |
| --- | --- | --- | --- |
| 36 | `graphite-core` extraction. Acceptance: Prismattyc compiles against it behind a flag. The PRD also puts motion curves in the core. | G0–G9 and P1–P8. The core holds the light-cycle path fraction (G7). Duration stays in host config (`focus_border_animation_ms`). | This plan replaces the flag. Host PRs keep shipped behavior and do not add one. `chrome_style` stays as 0.3.0 shipped it. |
| 37 | `graphite-tui` chrome: Rail, Pane, Chip, Badge, StatusDot, TabStrip. | T2 (tab strip and status dot), T3 (rail and sidebar), T4 (pane header). | This plan splits the row. Styled snapshots and MB2090's visual sign-off stay the acceptance, checked on these T rows. |
| 38 | Keymap registry, shortcut overlay, and command palette. | T5 is the overlay widget. G8 packs already-resolved runs. | This plan. Chord resolution stays in Prismattyc (`keybind::KeyMap`). The command palette stays in the host (§3.2). Neither becomes a core type. |
| 39 | Light-cycle and sub-cell raster. | G7 is the sample path. T4 is the virtual-clock widget. | This plan for the path and the widget. Sub-cell glyph raster is not a row here. It waits for the later Prismattyc-native backend. |
| 40 | Scorecard TUI skeleton: gate rail, tabs with badges, status bar. | S1 is the gate rail only. | Scorecard's own sequence and review. Tabs, badges, and the status bar are later Scorecard work after T2. This sign-off does not merge them. |

T6 (lunatui's `graphite` feature) and S1 land in those repos under their own review rules. This document specifies their acceptance so the dependency edges stay the ones in §4. Signing off here does not open or merge those PRs.

### 5.1 `graphite-core` repo

| PR | Title | Tests (written first) | Acceptance |
| --- | --- | --- | --- |
| G0 | Repository scaffold | CI builds the empty crate on Linux and macOS. A test asserts `rust-version` is 1.85. CI also runs `cargo +1.85 check --locked`. | MPL-2.0 `LICENSE`, `publish = false`, public GitHub repo, `cargo fmt`, `cargo clippy -- -D warnings`, `cargo doc` with no warnings. No lunatui dependency in `Cargo.lock`. A clone with no token succeeds. That public fetch is what P1's unauthenticated `--locked` build relies on. |
| G1 | Tokens, bar presets, WCAG color | WCAG anchors (black on white is 21; the existing `0x777777` point). Every `DARK` and `LIGHT` field pinned to the values in `graphite.rs` on `2a4dd0f`. Bar-preset table from the design brief (Graphite, Harbor, Moss, Plum/Sand). `step_bar_color` wraps, and the theme-bars cycle includes `None`. Accent reaches 3:1 on both bar luminances. Text pairs meet 4.5:1 where today's `token_text_pairs_meet_wcag_aa` and `bar_color_presets_meet_brief_contrast` require it. | Public tokens match the brief. No `Theme` type. No font crate. |
| G2 | `ThemeSource` and derivation | Brief ids with unchanged chrome colors return `DARK` or `LIGHT` verbatim. A non-brief fixture's derived tokens are pinned field by field against today's `derive_tokens`. An explicit bar preset replaces both bar fills. Readability runs again only off the brief. A chrome override that changes `chrome_bg` or `chrome_fg` leaves the brief path. | Same results as `prismattyc_themes_keep_the_brief_tokens`, `other_themes_derive_graphite_tokens_from_the_theme`, `explicit_bar_color_overrides_theme_bars`, `chrome_overrides_reach_graphite`, and `derived_text_stays_readable`. |
| G3 | Scale, `Rect`, `TextMetrics`, ellipsize | Scale 1000 and 2000 match `ChromeGeom::px` for the design constants, including the 220 px side rail at 18 columns. Ellipsize tests use a fake measurer (one cell per scalar) and cover short text, exact fit, and a cut. `Rect::contains` covers edges. | Core has no fontdue. A fake measurer is enough to lay out. |
| G4 | Tabs-bar model | With the fake measurer, core tests cover the decisions in `layout_and_hit_test_agree`, `narrow_bar_drops_the_command_field_then_shortens_labels`, `retina_layout_doubles_the_bar`, and `shift_bar_keeps_hits_on_the_moved_chips`. A core test covers `DropTarget` the way `drop_target_highlights_the_tab_or_the_new_tab_slot` does (tab slot versus new-tab slot), without painting. Hits are the core enum. | Command field drops before labels shrink. A shifted bar hits the shifted chips. `scale_milli` is a field. `ChromeGeom` is not. Fontdue parity stays on the host tests in P4. |
| G5 | Rail and side-column geometry | Copy `side_column_is_220px_at_the_default_width_and_tracks_the_grip` into the core against the core API. The host test stays. Chip height grows when pane names are on. Header, footer, gap, pad, and thumb match the current constants at scale 1000 and 2000. `side_cols_for_px` round-trips with `side_rail_px` inside the host's column cap. | No paint. No `SpaceRail` type. |
| G6 | Sidebar model | Copy these host tests into the core against the core API. The host copies stay for P6: `sidebar_panel_splits_title_list_and_three_actions`, `sidebar_scroll_clamps_and_thumbs_overflow`, `sidebar_rows_in_view_follow_scrolled_layout_slots`, `sidebar_header_puts_buttons_right_of_crumb`, `arrange_control_is_a_fixed_track_of_equal_segments`, `sidebar_hit_prefers_thumb_then_rows_then_buttons`, `sidebar_max_scroll_is_the_last_page_offset`. | Thumb wins over a row. Arrange is three equal segments. Actions stay `+ New tab`, `+ New space`, `Commands`. |
| G7 | Pane model and light-cycle path | `PaneStatus::decide` priority table (attention, mail, unseen, running, focused, quiet). `Dot::for_tab` priority. Handle rect covers the dot and the name and stops before the status. Activity rects cover the dot and the status label. Ring samples start at the top-left, proceed clockwise, and keep even arc steps (`ring_sweep_starts_top_left_runs_clockwise_with_even_arc_steps`). | No pixel buffer in the core. `RingSweep::paint` is not in this crate. |
| G8 | Legend packing and theme-picker frame | Given measured run widths, a group that does not fit is omitted (the rule behind `legend_keys_drop_a_shortcut_that_does_not_fit`). Theme picker is 760×460, and a scrolled hit returns the absolute row (`theme_picker_geometry_is_fixed_and_scrolled_hits_are_absolute`). | Inputs are label, chord, and selected. No palette row type and no theme catalog. The notice string drawn by `legend` stays a host painter. |
| G9 | Translucency role table | Each role in §3.1 maps to opaque or follow-chrome-alpha. A test lists the brief's opaque set and the follow set. | The table is data. No blend function and no `bar_alpha` writer. |

G0 through G3 are sequential. G4 through G9 are sequential after G3, in that order, because the tabs bar is the layout most likely to need a follow-up before the rail copies its shape.

### 5.2 Prismattyc host

Each host PR pins `graphite-core` to the revision merged for the matching G PR. The pin changes in the same PR that switches the call.

| PR | Title | Depends on | Tests that must stay green | Acceptance |
| --- | --- | --- | --- | --- |
| P1 | Git dependency only | G0 | The red check is `cargo tree -p prismattyc-host -i graphite-core`. It fails on the base because the package is absent, and it passes once the pin exists. `cargo test -p prismattyc-host` still compiles the graphite modules. | No call-site change. Lockfile pins one public rev. No lunatui crate in the tree. A clean checkout with no GitHub credential passes `cargo check --workspace --locked` and `cargo build --release --locked --workspace --bins`. |
| P2 | Color helpers forward to the core | G1, P1 | Existing `contrast_ratio_matches_wcag_reference_points`, `derived_tab_active_bg_stays_subtle_on_every_builtin`, `hover_rgb_stays_subtle_and_composes_over_active_fill`, and the raster contrast tests. | Host functions are one-line forwards. Classic callers are unchanged. Pixel tests do not move. |
| P3 | Token derivation | G2, P2 | `token_text_pairs_meet_wcag_aa`, `light_chips_and_bars_stay_light_with_a_visible_active_outline`, `accent_keeps_three_to_one_on_both_bars`, `bar_color_graphite_is_identity_and_cycle_wraps`, `bar_color_presets_meet_brief_contrast`, `bar_color_preset_paints_both_bar_grounds`, `prismattyc_themes_keep_the_brief_tokens`, `other_themes_derive_graphite_tokens_from_the_theme`, `explicit_bar_color_overrides_theme_bars`, `chrome_overrides_reach_graphite`, `derived_text_stays_readable`, `handle_tokens_match_the_brief`. | Host `theme_tokens` / `bar_tokens` / `accent` / `step_bar_color` call the core and the old bodies are gone. `BarColor` still deserializes in config. |
| P4 | Tabs-bar layout and hits | G4, P3 | `layout_and_hit_test_agree`, `narrow_bar_drops_the_command_field_then_shortens_labels`, `retina_layout_doubles_the_bar`, `shift_bar_keeps_hits_on_the_moved_chips`, `drop_target_highlights_the_tab_or_the_new_tab_slot`, `render_window_tests::graphite_tabs_bar_clicks_open_their_actions`, and `graphite_bar_buttons_run_open_space_new_tab_and_command_palette`. | `mux.rs` stores the core layout (with host scale beside it if paint still wants `ChromeGeom`). `bar_hit` mapping covers every variant the core returns. Paint stays in the host. |
| P5 | Rail and side-column geometry | G5, P4 | `side_column_is_220px_at_the_default_width_and_tracks_the_grip`, `space_rail` tests `graphite_side_list_scrolls_every_space_instead_of_dropping_chips` and `graphite_horizontal_bar_still_drops_chips_that_do_not_fit`, `chrome_contract_tests` side-rail width, `graphite_side_rail_is_220px_and_classic_columns_stay_in_cells`. | `space_rail.rs` and `rail_resize.rs` call the core. Classic column math stays in cells. |
| P6 | Sidebar layout and hits | G6, P5 | The sidebar tests named on G6, plus the paint tests `sidebar_paint_marks_the_selected_row` and `focused_session_row_shares_the_tab_accent` (they still paint through the host). | Clicking a session row still focuses the pane (#175). Collapse and width behavior from #174 stays. |
| P7 | Pane model and light-cycle path | G7, P6 | `activity_header_rects_cover_the_dot_and_the_status_label`, `pane_handle_zone_covers_dot_and_name_only`, `ring_sweep_starts_top_left_runs_clockwise_with_even_arc_steps`, `sweep_paints_trail_head_and_neutral_remainder`, `pane_chrome_sweep_settles_into_the_static_ring`, `pane_chrome_rings_the_focused_pane_and_outlines_the_rest`, `pane_chrome_handle_hover_paints_fill_and_outline`, `drag_helpers_paint_inside_the_frame`. | Sampling comes from the core. Stamping stays in the host. `border_underlay` tests still see one stroke after restore. |
| P8 | Legend and theme-picker geometry | G8 plus G9's role table consumed by the painters only as a documented constant, not a behavior change | `legend_keys_draw_focus_color_and_bar_keycaps`, `legend_keys_drop_a_shortcut_that_does_not_fit`, `legend_notice_does_not_draw_keycaps`, `theme_picker_geometry_is_fixed_and_scrolled_hits_are_absolute`, `palette_fixed_layout_does_not_follow_match_count_and_filter_hits_align`, `graphite_space_menu_click_opens_and_outside_click_closes`. | Palette contents, splash, and walkthrough captions stay in the host. Painters still force opaque ink on the roles in G9. |

P8 is the last Prismattyc PR in this sequence. After it, `rg` on the host should find no `fn bar_layout`, `fn derive_tokens`, `struct Tokens`, or `fn sidebar_layout` defined in `prismattyc-host`. The PR body includes that search.

### 5.3 `graphite-tui`, lunatui, and Scorecard

These PRs do not start until lunatui has theme tokens and `Widget`. They are not Prismattyc PRs, except that T-PR reviews check the forbidden `cargo tree` edges.

| PR | Repo | Tests | Acceptance |
| --- | --- | --- | --- |
| T1 | `graphite` | Token map snapshots for dark and light, truecolor and 16-color. A build test fails if `prismattyc` appears in `Cargo.lock`. | `graphite-tui` depends on `graphite-core` and lunatui only. Opens only once that CI can fetch lunatui. Until lunatui is public, the job uses a read credential. Prismattyc's public workflows do not run it. |
| T2 | `graphite` | Styled snapshots of the tab strip and status dot: idle, hover, active, working, unseen, attention, zoomed, narrow overflow. Two widths. | Uses core `TabText` and `Dot`. |
| T3 | `graphite` | Snapshots of the rail, the side list (scrolled), and the sidebar tree, at two sizes. | Uses core layout. Selection is a core index. |
| T4 | `graphite` | Virtual-clock snapshots of the pane header and light-cycle at 0, 70, 140, 210, and 280 ms, which is the host's default `focus_border_animation_ms`. Reduced motion is the settled ring on frame 0. | The widget takes a duration. The path fraction comes from the core. |
| T5 | `graphite` | Overlay snapshots: a run that fits, a run that the packer drops, and a notice string that the widget paints as text with no keycaps. | Chord labels arrive already resolved. The notice is a string, matching host `legend`. |
| T6 | `lunatui` | `cargo tree -p lunatui` has no graphite crate. `cargo tree -p lunatui --features graphite` shows `graphite-tui`. Default feature tests still pass. | Feature name is `graphite`. Default features are unchanged. |
| S1 | Scorecard | One term-level test: `scorecard tui` with the flag on draws the gate rail and exits. The default CLI command list is unchanged, and `cargo tree` of the default `sc-cli` build has no lunatui edge. A default `cargo fetch --locked` makes no request to lunatui. | Flag off by default. The default build stays on Rust 1.85 and does not list lunatui in the manifest. The flagged build uses Rust 1.88 (PRD D7) and runs only where CI can fetch lunatui. This is the first consumer. It does not merge inside Prismattyc. |

T2 through T5 can follow T1 one at a time. T6 follows T1 (a re-export can land as soon as the crate builds; widgets can follow). S1 follows T3, which is the first widget Scorecard's gate rail needs. T4 and T5 can land after S1.

## 6. Risks

| Risk | Why it is real | What keeps it small |
| --- | --- | --- |
| Layout numbers drift | `bar_layout` measures with fontdue, kerning, and ellipsis. A fake measurer in core tests checks the arithmetic only. | The host `TextMetrics` impl stays on fontdue. P4 runs the existing host layout tests unchanged. The core does not reimplement the font. |
| Theme contrast drifts | `derive_tokens` calls `hover_rgb` and `derived_tab_active_bg`, and then `keep_text_readable`. A rewritten mix will change screenshots. | G1 and G2 pin field values. P2 moves the helpers as forwards. P3 keeps the current token tests, which compare concrete RGB. |
| Hit mapping drops a case | `StripHit` is wider than the Graphite bar. A partial map compiles if the host still has a wildcard. | P4's mapping is exhaustive on the core enum. The click tests in `render_window_tests` and `main.rs` stay on the list. |
| The public API grows Prismattyc types | `TabInfo`, `PaneId`, `KeyMap`, and `Theme` are convenient and wrong. | Review rejects any core signature that names those types. Inputs are labels, flags, colors, and indexes. |
| Classic chrome moves by accident | P2 touches helpers that classic also calls. | P2's diff is the forward plus imports. Classic tests listed on that row stay green. Later PRs do not edit classic layout. |
| Two copies of a function | A host wrapper that still contains the old body will diverge. | Each P-PR deletes the old body. The P8 search is the check. |
| MSRV split | Prismattyc is 1.90. Scorecard's default CLI is 1.85. Lunatui's PRD targets 1.88, and that is the toolchain for the flagged TUI job. | G0 runs `cargo +1.85 check --locked` on the core. `graphite-tui` is not a Prismattyc dependency. S1's flagged job is the 1.88 build. |
| Font license leaves the host | The Plex blobs are SIL OFL and easy to sweep into a new crate with the painter. | Painters and `include_bytes!` stay in `prismattyc-host`. Review rejects a `fonts/` directory in the Graphite repo. |
| Pixel parity is promised to Scorecard | Cell widgets cannot reproduce SDF rounded rects or subpixel text. | `graphite-tui` acceptance is styled snapshots. The plan says the two renderers share models and tokens, and they do not share pixels. |
| Scorecard's batch CLI grows a TUI | `sc-cli` is clap and anstyle today. A feature-off git dependency can still be fetched. | S1 leaves lunatui out of the default manifest. The default `cargo tree` has no lunatui edge, and a default fetch does not contact lunatui. |
| Private lunatui fetch | lunatui is private, and `graphite-tui` depends on it. | The Graphite repo is public so P1 needs no token. T1 and the flagged S1 job wait until their CI can fetch lunatui, with a read credential until lunatui is public. |
| crates.io name moves | The 2026-10-05 404s were not reservations, and this session could not recheck (HTTP 403). | `publish = false` and git pins. The publish PR rechecks the index and is a separate sign-off. |
| Extraction PR while chrome is still moving | Sidebar width and session focus just landed (#174, #175). A parallel chrome feature will conflict with P6. | One host extraction PR at a time. Chrome feature work rebases onto the latest P-PR or waits. Behavior changes are not folded into a move PR. |
| Trunk review load | Eight host PRs each need independent review and a version bump. | The split is the point of the plan. Combining two rows to save a review reintroduces the large PRs #164 warned about. |

## 7. Open questions

| # | Question | Recommendation |
| --- | --- | --- |
| Q1 | GitHub repo name. `graphite` sits next to the Graphite editor's brand. #164 preferred a single crate `moonbase-graphite`. #169 preferred `prism-chrome`. | Use `github.com/moonbase2090/graphite` with crates `graphite-core` and `graphite-tui`, as D11 already does. `prism-chrome` would tie the look to one host and fight the Scorecard consumer. A single crate would put lunatui on the core graph. Confirm the org repo name at G0. |
| Q2 | Should P2 retarget classic color helpers, or should the core vendor its own WCAG copy and leave `raster.rs` alone? | Forward the helpers. One formula is the way to keep Graphite derivation and classic contrast from drifting. The PR stays behavior-preserving and lists the classic tests. If review wants classic untouched, the fallback is a duplicated formula plus a differential test against `raster::contrast_ratio` on a fixed set of pairs, deleted when a later PR forwards. Prefer the forward. |
| Q3 | When does `graphite-tui` start relative to Prismattyc? | After G3 the models are real enough to design widgets, and the code waits until lunatui PRs 26 and 28 have landed. Prismattyc P1–P8 do not wait on lunatui. |
| Q4 | When to publish to crates.io. | After P3 (Prismattyc derives tokens from the published revision) and T1 (a second crate maps those tokens). Still a separate PR, still `publish = false` until that PR. Recheck crates.io in that PR. |
| Q5 | Does this sign-off PR bump Prismattyc's workspace version? | No. It is docs-only and it is not a merge request. The version bump belongs to each later code PR that merges, per `CONTRIBUTING.md`. |
| Q6 | Should Prismattyc grow a `tests-first` CI job like lunatui's? | Not in this sequence. Each code PR links the red commit in the body. A CI job that enforces it would be its own trunk PR if MB2090 wants it after the first extraction PR shows the habit. |
| Q7 | Where does `BarLayout` live once `mux.rs` stores it? | The core owns the layout struct. `mux.rs` stores that struct. Scale for painters is `scale_milli` on the layout. `ChromeGeom` remains the host's classic-or-graphite flag and is not a field of the core layout. |
| Q8 | Who is the second consumer if Scorecard's TUI slips? | Prismattyc is consumer one. `graphite-tui`'s snapshot tests are consumer two for the models. Scorecard S1 is still required before calling the extraction finished, because D11 names Scorecard. Slipping S1 does not revert P1–P8. |

## 8. What sign-off authorizes

Signing off this document authorizes G0–G9 and P1–P8, in that order, under the acceptance lines above and under §5.0. T6 and S1 keep the acceptance in this plan and open under lunatui's and Scorecard's own review. This sign-off does not merge them. It does not authorize a pixel-painter crate, a ratatui dependency, a Prismattyc → lunatui dependency, a default-on Scorecard TUI, a private `graphite-core`, or a crates.io publish.

This pull request stays open for that sign-off. Merging it is a separate decision, and the author of the plan does not merge it.
