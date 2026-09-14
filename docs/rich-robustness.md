# Rich overlay robustness

**Status:** Frozen regression pin while rich product vision is open. Not a
PRD §5.6 / A-6 pass and not a Phase 3 product checkpoint.
**Rules:** [hybrid-rendering.md](hybrid-rendering.md) z-order, clip, alt-screen
suspend, teardown.

## Matrix

| Case | Automated | Live repro (owner box) |
|------|-----------|------------------------|
| Resize re-clip | `raster::tests::overlay_paint_clips_after_pane_shrink`, `rich::tests::resize_below_region_hides_rows_past_grid` | `prismattyc-host --experimental-rich` + a rich client; shrink the pane until a cell-rect region is wider than the content rect. Ink must stop at the pane edge (gap/chrome stay chrome). |
| Scroll translate+clip | `rich::tests::scroll_translates_then_detaches_fully_above`, `raster::tests::overlay_paint_clips_partially_scrolled_region` | Attach a cell-rect, then scroll the pane. Region rides the grid and clips at the content rect; fully off-screen detaches. |
| Alt-screen suspend | `rich::tests::paint_policy_skips_primary_overlays_on_alt` | Run `vim` or `less` in a rich pane. Overlays vanish on enter, return on exit; they are not detached. |
| Pane close teardown | `mux::tests::closing_a_rich_pane_drops_its_regions` | Split, attach on one pane, C-S-W that pane. The survivor must not keep the dead region's paint. |
| Two regions / z-order | `rich::tests::two_regions_keep_independent_z_and_damage`, `raster::tests::viewport_overlay_paints_above_cell_rect` | Cell-rect (z1) + viewport HUD (z2). HUD sits above; updating the cell-rect must not rewrite the HUD. |
| Flood + rich active | `rich::tests::flood_with_live_region_updates_stays_responsive` | `yes` in a rich pane while a region ticks. Grid keeps painting; host stays `Wait`-silent when output stops. |

The pin is `scripts/test-phase3-rich.sh`. It is not a required light
Local Actions gate. Run it by hand, with
`local-actions run --event pull_request --job phase3-rich`, or via the
scheduled `phase3-rich-nightly` workflow.
