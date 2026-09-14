# Host interaction feedback (the "rich experience" reference)

**Status:** Reference. Owner framing 2026-08-29: hover, active, drag, and
attention feedback in `prismattyc-host` are one visual-feedback family.
This page is the single place that says what feedback every interactive
element gives in every state, which token paints it, and which ticket
owns it. Chrome work cites this page; the walkthrough spike (PT-82) reads
it for its captions.

**Scope:** the windowed host (`prismattyc-host`, winit + raster). The TTY
client (`pmux-attach`) has no pointer and no hover; its states are the
attach chrome strings in `docs/mux-cli.md`.

## 1. Tokens

All colours come from the active theme (`crates/prismattyc-host/themes/*.toml`,
`theme.rs`) or a blend of two theme colours. Nothing in the feedback family
uses a literal colour.

| Token | Role | Ghost value |
|---|---|---|
| `chrome_bg` / `chrome_fg` | strip, rail, footer ground and ink | `#1d1f21` / `#ffffff` |
| `default_bg` / `default_fg` | pane ground and text | `#282c34` / `#ffffff` |
| `overlay_bg` | palette, find prompt, detail box, toast ground | `#3e4451` |
| `pane_border` | inactive pane outline | `#666666` |
| focus colour | `focus_border` config (`amber` = `#ffb454`; palette in `raster.rs` `FOCUS_BORDER_PALETTE`) | amber |
| `unseen_badge` / `mail_letter` | new output, mail letter | `#f0c674` |
| `active_badge` | output flowing now | `#b5bd68` |
| `attention_badge` | agent attention, bell | `#ff6b6b` |
| `selection_bg` / `selection_fg` | inverse selection, selected palette row, rename edit | `#ffffff` / `#282c34` |

Blend factors (shared in `raster.rs` and `theme.rs`):

| Factor | Default | Used by |
|---|---|---|
| `TAB_MARKER_OPACITY` | 0.70 | current space chip marker on the rail (exists) |
| `ACTIVE_TAB_MARKER_ALPHA` | 128/256 | active-tab bottom border: white at 50 % alpha over the chip (PT-143) |
| active-tab background | dark: +8 % toward `chrome_fg`; light: −6 % | PT-95 |
| hover | +10 % toward `chrome_fg` (dark), −8 % (light); config `hover_blend` 0.0–0.3 | PT-96 |
| pane opacity | `pane_opacity_active` / `pane_opacity_inactive` (1.0 / 1.0) | PT-83 (merged #112) |
| window opacity | `window_opacity` 1.0 | PT-87 |

Rule: a state composes on top of the state below it (hover over active over
idle). Deltas stay subtle — raster tests bound them per channel (PT-95:
8–40, PT-96: 4–30).

## 2. States

| State | Meaning | Who sets it |
|---|---|---|
| idle | nothing happening | — |
| hover | pointer over the element | pointer target transitions (PT-96) |
| pressed / drag | mouse down; drag after press-and-move | PT-69 drag, scrollbar |
| active / focused | the selected tab, the focused pane, the focused rail chip | keyboard or click |
| attention | something wants the human: unseen output, active output, agent attention, mail, bell | PT-53, PT-30, bell |
| editing | inline rename in progress | C-S-R, double-click |
| gone | the pane's child exited (placeholder) | PT-68 |

## 3. Element matrix

Columns are the states above. "—" means no change from idle. Ticket ids in
parentheses are not merged yet.

### Tab chip (title row + PT-78 handle row)

| State | Feedback |
|---|---|
| idle | `chrome_bg` ground, `chrome_fg` title, × in `chrome_fg`; badges at the right: unseen (`unseen_badge`), active (`active_badge`), attention (`attention_badge`) |
| active | `tab_active_bg` ground = `chrome_bg` lifted 10 % (`ACTIVE_CHIP_LIFT`; overridable); title in the chip's contrast ink; 1-px white line at 50 % alpha under the chip (`ACTIVE_TAB_MARKER_ALPHA`); the chip itself lightens by `ACTIVE_TAB_GRADIENT_DEPTH` toward the top. Pane chips: white 35 % fill (60 % focused) with a 1-px white 50 % outline |
| hover | chip ground brightened by `hover_blend`; cursor `pointer` (PT-96) |
| pressed / drag | chip follows the pointer; drop targets outline; toast "Moving tab NAME → …" (PT-79); cursor `grab` |
| editing | title cell inverse (`selection_bg` / `selection_fg`), caret |
| click zones | title row and empty handle row select; × closes (last tab: Detach semantics, PT-90); middle-click closes |

### Pane handle (one cell per pane, multi-pane tabs)

| State | Feedback |
|---|---|
| idle | filled cell in `chrome_fg` at reduced opacity; the focused pane's handle in the focus colour |
| hover | brightened by `hover_blend`; cursor `pointer` (PT-96) |
| click | selects the tab and focuses that pane (PT-90) |
| drag | press-and-move; handle ghost follows; toast "Moving pane N · SESSION → tab …" (PT-79); drop on a tab moves, on the empty end makes a new tab |

### Pane

| State | Feedback |
|---|---|
| idle (inactive) | outline in `pane_border`; ground blended at `pane_opacity_inactive` (PT-83) |
| focused | outline in the focus colour; optional light-cycle trace on focus change (`focus_border_animation`); ground at `pane_opacity_active` |
| unseen / active / attention | corner badge in the matching badge colour; the tab aggregates them |
| bell | toast chip " bell " on the pane (`bell_toaster`, 10 s), optional sound and OS notification |
| mail | sticky "mail" cell in `mail_letter`; doorbell types `PMUX_MAIL` into an agent CLI only when the pane is idle (PT-85, PT-94) |
| zoomed | fills the tab; strip reads `[NZ]`; leaves on any topology change (PT-57) |
| gone | placeholder: session name, exit reason, "Enter to reopen" (PT-68) |
| hover | none (pointer belongs to the guest; ADR-0003 mouse rules) |

### Scrollbar

| State | Feedback |
|---|---|
| idle | thumb in `chrome_fg` at reduced opacity in a constant gutter (PT-80) |
| hover | thumb brightened (PT-96) |
| drag | thumb follows; viewport scrolls; "scroll N/M" chip |

### Command palette (PT-92, direction C)

| State | Feedback |
|---|---|
| idle | `overlay_bg` box, focus-colour frame; query row; filter chips; RECENT / MATCHES headers in muted ink |
| selected row | inverse (`selection_bg` / `selection_fg`) |
| active filter chip | inverse |
| detail box | `overlay_bg`, full description, chords in the focus colour, config key |
| footer | key hints in the focus colour |

### Spaces rail (PT-91)

| State | Feedback |
|---|---|
| idle | label-sized chips, left-aligned, capped by `space_rail_chip_cols` |
| current space | chip on the active-chip colour (focus colour toned toward `chrome_bg`, WCAG AA ink), marker, dot instead of `×` — same as the active tab (PT-136) |
| hover | brightened (PT-96) |
| editing | inline rename, inverse |
| × | delete after the PT-89 confirm row |
| + | compact chip; opens the modal save-space name editor |

### Pane dividers (PT-133)

| State | Feedback |
|---|---|
| hover | cursor `col-resize` / `row-resize` over the gap between two panes (± 3 px) |
| drag | the split ratio follows the pointer every motion event; panes re-fit live (PTY resize); a pane never shrinks below its minimum |
| release | ratio stays; cursor returns to default |

### Toasts and prompts

| Element | Feedback |
|---|---|
| bell toast | pane-anchored chip, `attention_badge`, lingers `bell_toaster_ms`, click dismisses |
| write-fail toast | same chip, label ` input disconnected — reopen the pane `; shows even when `bell_toaster` is off (PT-119) |
| drag toast (PT-79) | focus-colour chip at the bottom-right (above a bottom rail): `Moving tab NAME → tab OTHER` / `→ new tab` / `→ (no target)`, `Moving pane N · SESSION`; updates with the pointer, clears on drop; `drag_toaster` |
| find prompt | bottom-left inverse prompt, `n/m` counter |
| walkthrough caption (PT-82) | translucent subtitle overlay near the bottom, × dismiss, never takes input |

## 4. Cursor shapes (PT-96)

| Over | Cursor |
|---|---|
| pane | guest decides (text by default) |
| tab chip, pane handle, ×, rail chip, + | `pointer` |
| while dragging a chip or handle | `grab` |
| scrollbar thumb | `default`; `grab` while dragging |
| hyperlink (OSC 8) | `pointer` (exists) |

## 5. Rules

1. Feedback is additive and subtle: hover over active over idle, each a
   bounded blend of theme colours; never a literal colour.
2. Everything interactive answers the keyboard too; hover is a hint, not
   the only path (PT-34 accessibility stays reachable).
3. A state that the host paints must be a state the host handles: no dead
   zones (PT-90), no painted-but-inert chrome.
4. Repaint only the band that changed on hover. Never repaint on raw pointer
   motion inside one target.
5. Reduced motion: no animation for hover or drag; the focus-border trace
   is already opt-in.

## 6. Where the code lives

| Piece | File |
|---|---|
| tokens and theme fields | `crates/prismattyc-host/src/theme.rs`, `themes/*.toml` |
| strip, badges, marker, handles | `crates/prismattyc-host/src/raster.rs` `rasterize_tab_strip_with_theme` |
| pane chrome, focus border, light cycle | `raster.rs` `rasterize_pane_chrome_with_theme`, `trace_border_trail` |
| toasts, find, palette, scrollbar | `raster.rs` `rasterize_bell_toast`, `rasterize_find_prompt`, `rasterize_palette`, `rasterize_scrollbar` |
| hit-testing, drag, hover state | `crates/prismattyc-host/src/main.rs` (`handle_strip_click`, `strip_drag`, `pointer_px`) |
| attach-tab cache and space files | `crates/prismattyc-mux/src/attach_tabs.rs`, `layout_file.rs` |

Tickets in the family: PT-57, PT-68, PT-69, PT-78, PT-79, PT-80, PT-83,
PT-85, PT-86, PT-87, PT-89, PT-90, PT-91, PT-92, PT-94, PT-95, PT-96.
