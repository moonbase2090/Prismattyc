# Prismattyc UI refresh: design brief, direction A (Graphite)

**Design:** Prismattyc UI refresh, direction A (Graphite), v7 (private design project; boards are referenced by name below).

**Status:** MB2090 chose **A · Graphite** and reviewed it on Oct 4, 2026. This version adds the seven items from the Oct 4 review notes. Nothing A already had was restyled.

## Where things are on the canvas

| Row | What it shows |
| --- | --- |
| Before | v0.2.29 captures (busy workspace, palette + orange hint bar) |
| A · Graphite | Original A: workspace, tab/focus/hint/toast sheet, theme dialog. **Unchanged.** |
| B · Soft Panels, C · Prism | The other explored directions, kept for reference (B's sidebar is the source of item 2) |
| A refinement · Spaces bar in every position | 10 boards: bottom, top, left, right, off × dark, light |
| A refinement · Optional combined sidebar | Dark and light |
| A refinement · Light-cycle, handles, arrangement | Live light-cycle (dark + light), pane-header handle states (dark + light), arrangement behavior (dark + light) |
| A refinement · Transparency | Transparency settings dialog (dark + light, interactive) and translucent workspaces (dark, light) |
| A refinement · Ctrl+Shift legend and bar color | The legend with the new key, plus bars in four colors (dark + light) |
| A shared parts | Pane grid, tabs bar, legend, spaces bar/rail and sidebar. Every refinement board is built from these, so A looks identical everywhere. |

## Kept exactly as designed

- Pane treatment and pane chips: an 8 px radius, a 28 px title row with status dot + name + meta, and status on the right (new output, mail + count, running, focused).
- Top tabs bar: release-lab dropdown, tabs with status dots, the "needs you" badge, `+`, and the "Run a command" field with `Ctrl Shift P`.
- Tab state chips and their descriptions: Idle, Hover, Active, Working, Unseen output, Mail waiting, Attention, Zoomed pane.
- The one-row Ctrl+Shift keycap legend. One key is added (item 5).
- The bottom spaces bar (default position).
- The 2 px pane focus ring (1 px border + 1 px outside) with a tinted title row.

## What this pass adds

### 1. Spaces bar in every supported position (dark + light)
`space_rail` = `bottom` (default) | `top` | `left` | `right` | `off`. The tabs bar has no position setting today, so it stays at the top in every board.
- **Bottom:** A's bar as designed, with Space chips on the left and counts and the hint on the right.
- **Top:** the same bar, unchanged, above the tabs bar (this matches today's order). Its hairline moves to the bottom edge.
- **Left / right:** a 220 px column (`space_rail_width_cols`). The 44 px header lines up with the tabs bar. Each Space is a two-line chip, with the name on the first line and live pane names (`space_rail_pane_names`) underneath. The list **scrolls inside a fixed-width column**, so all 13 Spaces stay reachable, instead of today's behavior where chips that don't fit are dropped. The counts and the "Hold Ctrl Shift" hint sit in the footer. The inner edge is a resize grip with the `col-resize` cursor.
- **Off:** no bar. The release-lab dropdown in the tabs bar is still the way to switch Spaces.

### 2. Optional combined sidebar (a user setting, from B)
Proposed key: `layout = "sidebar"` (default `"bars"`). One 256 px tree replaces the top tabs bar and the bottom spaces bar: Spaces → tabs → panes, with A's status dots, the mail icon + count, the "needs you" badge, `+ New tab`, `+ New space`, and Commands (`Ctrl Shift P`) at the bottom. The tree scrolls inside a fixed-size panel. The active tab uses A's active fill with a 2 px accent marker. A 44 px header over the panes shows the breadcrumb and the arrangement buttons. A's default layout doesn't change.

### 3. Light-cycle stays
It lives on the pane focus ring and runs on every focus change. Focus itself is instant (title tint, bright name), and only the ring animates. It draws clockwise from the top-left corner along A's 8 px radius, and a 3 px head (dark `#d6e8ff`, light `#163f80`) settles into A's 2 px accent ring after `focus_border_animation_ms` (280 ms default). Reduced motion makes the ring appear instantly. The board animates live, with Pause and "Slow motion ×5" buttons and still frames at 0/70/140/210/280 ms. Renderer note: the ring and head fit inside the existing 7 px `BorderUnderlay` budget. Following the rounded corner needs arc-length stepping on the corner arcs, which is cheap to precompute per pane size.

### 4. Transparency
- **Settings dialog** in A's dialog frame (760×460, fixed size, the list scrolls inside, live preview on the right): Window opacity (`window_opacity`), Chrome opacity (`chrome_opacity`, "Follow window opacity" by default), Window blur (`window_blur`), Active / Inactive pane opacity (`pane_opacity_active` / `_inactive`), Background image + Image opacity (`background_opacity`) + Image blur (`background_blur_px`). The platform limits are written into the dialog: X11 without a compositor and `--gpu` stay opaque, and lowering from 1.0 on X11/Wayland needs a restart.
- **Translucent workspaces** (dark at 0.82, light at 0.84, inactive panes 0.85, blur on) over a busy desktop. Text, the cursor, status dots, the "needs you" badge, the active-tab chip, the command field and the focus ring stay opaque. Only the ground, pane surfaces and bar backgrounds go translucent, which matches how the renderer already treats `window_opacity`.

### 5. Bar color hotkey
**Ctrl+Shift+B** cycles the bar background color (free today; `[` and `]` keep cycling the focus-ring color). It's added to the legend right after "[ ] Focus color". Proposed config `bar_color` and palette actions `bar_color_next` / `bar_color_prev`. Both bars change together.

| Preset | Dark tabs bar / spaces bar | Light tabs bar / spaces bar |
| --- | --- | --- |
| Graphite (default) | `#15181d` / `#0d0f12` | `#eef0f3` / `#e4e7ec` |
| Harbor | `#152131` / `#0e1722` | `#e3edf8` / `#d6e3f2` |
| Moss | `#17221b` / `#0f1712` | `#e4f0e7` / `#d7e7db` |
| Plum (dark) / Sand (light) | `#211a27` / `#17121c` | `#f4ece0` / `#ebe0cf` |

Contrast on every preset: tab text ≥6.7:1, spaces-bar text ≥4.6:1, accent underline ≥4.1:1.

### 6. Pane-header handles (hover and drag)
The handle is the dot + name already in A's pane header, so it looks the same at rest.
- **Hover:** the pointing hand, a hover fill behind the dot and name, and a matching hover outline on that pane (dark `#3d5f8f`, light `#9dbbe8`). Click focuses the pane and right-click renames it.
- **Drag:** a grab hand. The handle lifts as a small chip and leaves a dashed slot behind.
- **Drop on a tab:** the tab gets an accent outline and the pane moves into it.
- **Drop on the empty end of the tabs bar:** a dashed "New tab" target.

These match today's drag-to-move behavior and right-click rename.

### 7. Arrangement buttons (single, split, 2×2)
They're labeled **Arrange** in the tabs bar, before the command field, and each button has a text label and tooltip ("Arrange: 2×2 grid").
- **Bigger layout adds empty shells:** 1 pane → 2×2 adds 3 new shells, and 1 pane → split adds 1. Focus stays where it was.
- **Smaller layout never closes panes:** 2×2 → single (or 2×2 → split) zooms the focused pane. The other panes keep running, the tab reads "4 panes · zoomed", and a toast says "3 panes still running · Ctrl Shift Z restores".

## Standing rules (applied everywhere)

- **Pointing hand on every clickable element.** This covers tabs, the dropdown, Space chips and rail rows, sidebar rows, pane-header handles, arrangement buttons, dialog rows/buttons/toggles/sliders, and the light-cycle controls. Today's code still lacks hover targets for the scrollbar thumb (it returns `Default`, asserted at `main.rs:19450`), the theme picker rows, context-menu rows, dialog buttons and toast dismiss. Those need `HoverTarget` variants that map to `Pointer`. Exceptions: drags show `Grab`, and the rail resize grip shows `col-resize`.
- **Fixed-size panels and dialogs with scrolling inside.** This covers the side rail list, the sidebar tree, the theme and transparency dialogs, and the legend (one row). Today the theme picker (`main.rs:6035`), the Space pickers (`main.rs:6103`, `6152`), the session prompt (`session_prompt.rs:521`) and the restore prompt (`restore_prompt.rs:197`) still use `PaletteLayoutMode::ContentFit`. They should move to fixed geometry, like the command palette's `FixedHeight`.

## Tokens (A, unchanged, with the light set used on the canvas)

| Role | Dark | Light |
| --- | --- | --- |
| ground | `#101216` | `#e9ecf0` |
| tabs bar (`chrome_bg`) | `#15181d` | `#eef0f3` |
| spaces bar | `#0d0f12` | `#e4e7ec` |
| pane / focused pane | `#181b21` / `#1b1f26` | `#ffffff` / `#ffffff` |
| focused title row | `#1c2330` | `#eaf1fc` |
| active tab chip | `#252a33` | `#ffffff` + `#aab2bd` border (#145; was `#d5d9df`) |
| hairline | `#262a32` | `#d5d9df` |
| text / muted / tab text | `#e6e9ee` / `#a3abb8` / `#aeb5c1` | `#1f2329` / `#5b6472` / `#4a525e` |
| accent (= focus color) | `#5aa2ff` | `#2f6fd0` |
| working / unseen / attention | `#4cc98a` / `#f2b84b` / `#ff7a6b` | `#1f8a55` / `#b87a0a` (text `#9a6200`) / `#c2392b` |
| handle hover fill / hover outline | `#252a33` / `#3d5f8f` | `#e2e6eb` / `#9dbbe8` |
| light-cycle head | `#d6e8ff` | `#163f80` |

Prismattyc ships these tokens as themes (#145): **Prismattyc Dark** and **Prismattyc Light** set the terminal ground, text, cursor, and selection from the same column as the chrome, and **Prismattyc (match system)** (`theme = "prismattyc"`, the default for new installs) switches between them with the OS appearance. On Light, the tabs bar, spaces bar, and sidebar stay opaque under `window_opacity` so a dark desktop cannot grey them; panes and the window ground still go translucent.

Spacing, radius and type are unchanged from A: 8 px gaps, 6/8/10 px radii, IBM Plex Sans for the chrome, the user's mono font for terminal text, a 44 px tabs bar, a 30 px spaces bar, a 28 px pane title row, and a 760×460 dialog.

## What the renderer needs, ranked by impact vs effort

| # | Item | Impact | Effort | Renderer work |
| --- | --- | --- | --- | --- |
| 1 | Pane-header handle hover/drag states | High | S | Hit zone on the header name; hover fill; drag chip bitmap; reuse today's handle drag logic |
| 2 | Arrangement buttons + no-close shrink | High | S–M | 3 hit targets in the tabs bar; "smaller layout = zoom focused" in the layout actions; spawn shells to fill bigger layouts |
| 3 | Bar color hotkey | Med | S | New `bar_color` key + 2 actions; recolor chrome_bg and the spaces bar |
| 4 | Spaces bar positions in A's style | High | M | The existing `space_rail` geometry with the new chip style; a scrolling side list instead of dropping chips |
| 5 | Light-cycle on rounded ring | Med | M | Arc-length sweep along corner arcs within `BorderUnderlay` |
| 6 | Transparency dialog | Med | M | A new fixed-size overlay writing existing keys; live preview uses the existing opacity path |
| 7 | Combined sidebar layout | Med | L | A new `layout` mode: a tree view with hit-testing, scroll, and collapse state |

## Accessibility

- All text pairs on the canvas are ≥4.5:1 in both modes, including every bar-color preset. Focus rings and the accent underline are ≥3:1.
- Status is never color alone: dot vs hollow dot, a mail icon + count, "needs you" in words, and "new output" in words.
- Reduced motion: the light-cycle ring appears instantly. Nothing else animates.
- Everything stays keyboard-first: the Ctrl+Shift legend, `space_rail_focus`, the arrangement actions (the palette actions `layout_*` / `preset_*` already exist) and dialog keys (↑↓, Enter, Esc).

## Open questions for MB2090

1. Ctrl+Shift+B for bar color: is the key OK, and are the four presets right?
2. Combined sidebar: should the setting be called `layout = "sidebar"`, and should it remember collapsed Spaces per window?
3. Arrangement buttons: is their place in the tabs bar (before the command field) right, or should they appear only in the sidebar layout?

## Decisions (MB2090, Oct 4, 2026)

All three open questions use the defaults above: Ctrl+Shift+B with the four presets; `layout = "sidebar"` remembering collapsed Spaces per window; Arrange buttons in the tabs bar before the command field. Implementation is tracked in moonbase2090/Prismattyc#103 (sub-issues #104–#114), gated behind `chrome_style = "graphite"` until #114 flips the default.
