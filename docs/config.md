# Prismattyc host config file

Optional TOML config for the **windowed host** (`prismattyc-host`), hot-reloaded
while the host is running. The classic nested host (`prismattyc`) does not read it.

## Location

1. `$PRISMATTYC_CONFIG` (explicit path override)
2. `$XDG_CONFIG_HOME/prismattyc/config.toml`
3. `~/.config/prismattyc/config.toml`

On first run, `prismattyc-host` writes the full commented template when the
file does not exist. After that, the file is the source you edit. An invalid
file is ignored with a stderr message — at startup and on every reload —
so a half-saved or broken config can never take down a running host.

`prismattyc-host --write-config [PATH]` prints the template (`PATH` omitted
or `-`) or writes it. `--write-config --merge` keeps user values and
comments and appends missing keys. `pmux config init [--merge]` writes or
updates the `[mux]` section.

## Precedence

CLI flags and `PRISMATTYC_*` env vars always win over the file — at startup **and**
across hot reloads. A value pinned on the command line (`--focus-border`,
`--panes`, `--no-splash`) or by env (`PRISMATTYC_FOCUS_BORDER`,
`PRISMATTYC_HOST_FONT`, `PRISMATTYC_HOST_FONT_FALLBACK`, `PRISMATTYC_NO_SPLASH`)
never gets overwritten by a config edit. `splash = false` is the durable
host opt-out; the flag and env still hide the splash when `splash = true`.
The classic `prismattyc` binary does not read this file.

## Keys

```toml
# Prismattyc host config. Every key is at its default.
# Edit in place. CLI flags and PRISMATTYC_* env vars still win.

# -- appearance / theme --
# Named theme: built-in slug, display name, sibling themes/ file, or absolute TOML path. theme slug or absolute path.
theme = "prismattyc-default"
# Render timings and counters. off|osd|log|both.
render_timer = "off"
# Log every render frame when render_timer includes log; use for benches only. true|false.
render_timer_log_every_frame = false
# Show the launch splash on bare launches. true|false.
splash = true
# Animate the launch splash word art. true|false.
splash_animation = true
# Tab strip visibility: auto and always show one tab; multi needs two tabs. auto|always|multi.
tab_strip = "auto"
# Multi-pane title row: focused pane OSC title, or handle hover only. focused|hover.
pane_titles = "focused"
# Immediate hover blend for interactive strip, rail, and scrollbar chrome. 0.0-0.3.
hover_blend = 0.1

# -- focus border --
# Focus border color. coral amber yellow green blue violet ink, or 0-6.
focus_border = "blue"
# Focus-change animation. "none" | "light-cycle".
focus_border_animation = "none"
# Light-cycle sweep duration in ms. 50-5000.
focus_border_animation_ms = 280
# Draw the bright vehicle box at the sweep head. true|false.
focus_border_animation_head = true

# -- font --
# Primary font path; falls back to the built-in chain if unset or unreadable. absolute path.
# font = "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf"
# Extra fallback faces, tried after the built-in chain. array of absolute paths.
# font_fallback = ["/usr/share/fonts/TTF/SymbolsNerdFont-Regular.ttf"]
# Cell size in px before display scaling. 6-72.
font_px = 15.0
# Host-only OpenType ligatures for eligible terminal-grid text. true|false.
font_ligatures = false
# OpenType feature tags; prefix with '-' to disable. four ASCII alphanumeric characters or spaces.
font_features = ["calt", "liga"]

# -- layout --
# Initial pane count; startup only. 1-8.
panes = 1
# Window edge to pane chrome, physical pixels. 0-128.
window_padding_px = 3
# Space between pane chrome rectangles; used with 2+ panes. 0-128.
pane_gap_px = 3
# Pane chrome to terminal cells and tab content, physical pixels. 0-128.
pane_padding_px = 5
# Edge that shows the saved-spaces rail; off hides it. "bottom" | "left" | "top" | "right" | "off".
space_rail = "bottom"
# Save changed Space layouts after two idle seconds. true | false.
space_autosave = false
# Choose naming prompts, automatic sessions, or blank terminals. "ask" | "auto" | "blank".
session_naming = "ask"
# Startup choice; restore reconnects live sessions without launching stopped ones. "ask" | "restore" | "fresh".
space_startup = "ask"
# Fixed width of left and right Space rails; drag the edge to resize. 8-60.
space_rail_width_cols = 18
# Recreate blank terminal tabs, split layouts, and directories with fresh shells. true | false.
restore_blank_terminals = false
# Widest space chip in cells (chips fit their labels); 0 = 28. 0 or 6-40.
space_rail_chip_cols = 0

# -- bells and attention --
# Flash the window on BEL (~120ms invert). true|false.
visual_bell = true
# Play the bundled Zen bell on BEL. true|false.
audible_bell = true
# Show a toast on the pane that rang BEL. true|false.
bell_toaster = true
# Bell toast linger in ms. 500-60000.
bell_toaster_ms = 10000
# Show 'Moving tab NAME → target' while a tab or pane is dragged. true|false.
drag_toaster = true
# OS notification on BEL while the window is unfocused. true|false.
os_notify_bell = false
# Play the attention cue on OSC 9 / 777 / 99. true|false.
attention_sound = true
# Draw a coral attention badge on the tab. true|false.
attention_badge = true
# OS notification when the attention pane is not selected or the window is unfocused. true|false.
os_notify_attention = true
# Play bundled walkthrough narration; missing clips stay silent. true|false.
walkthrough_audio = true
# ElevenLabs voice name for scripts/walkthrough-voice.sh (generation only). voice name.
walkthrough_voice = "russ"

# -- background --
# Window background PNG; absolute path, PNG only. absolute path.
# background_image = "/usr/share/backgrounds/example.png"
# How much of the image shows (0 = flat theme bg, 1 = image). 0.0-1.0.
background_opacity = 0.35
# Box-blur radius in pixels. 0-64.
background_blur_px = 0
# Focused/zoomed pane surface opacity; try 0.6-0.8. 0.0-1.0.
pane_opacity_active = 1.0
# Host overlay surface opacity; try 0.6-0.8 (remove to follow pane_opacity_active). 0.0-1.0.
overlay_opacity = 1.0
# Every other pane surface opacity; try 0.6-0.8. 0.0-1.0.
pane_opacity_inactive = 1.0
# Window ground opacity; text and explicit cell backgrounds stay opaque. Hot reload works on macOS; other platforms may need a restart below 1.0. 0.0-1.0.
window_opacity = 1.0
# Tab strip and footer bar opacity; defaults to window_opacity. 0.0-1.0.
chrome_opacity = 1.0
# Blur behind translucent window grounds; use window_opacity below 1.0. Hot reload works on macOS; no-op where unavailable. true|false.
window_blur = false

# -- theme overrides --
# Recolour the named theme without copying a theme file. Uncomment a key
# to override it; values are #RRGGBB. Shown at the prismattyc-default
# values. Hot-reloaded with the rest of this file; the theme picker keeps them.
[theme_overrides]
# Terminal text. #RRGGBB.
# default_fg = "#d0d0d0"
# Terminal ground. #RRGGBB.
# default_bg = "#121214"
# Chrome text: tabs, rails, footer. #RRGGBB.
# chrome_fg = "#e5e9f0"
# Chrome ground. #RRGGBB.
# chrome_bg = "#1b1e26"
# Active tab chip fill; follows the focus colour when unset. #RRGGBB.
# tab_active_bg = "#2b2e36"
# Frame ground behind panes; pane opacity blends toward it. #RRGGBB.
# pane_backdrop = "#0a0a0c"
# Unfocused pane border. #RRGGBB.
# pane_border = "#454a57"
# Toast and overlay ground. #RRGGBB.
# overlay_bg = "#2a364a"
# Unseen-output tab badge. #RRGGBB.
# unseen_badge = "#ffb454"
# Mail envelope ink. #RRGGBB.
# mail_letter = "#ffb454"
# Working tab badge and breathing handle chip. #RRGGBB.
# active_badge = "#4cd18b"
# Agent attention tab badge. #RRGGBB.
# attention_badge = "#ff6b6b"
# Cursor text; set together with cursor_bg. #RRGGBB.
# cursor_fg = "#121214"
# Cursor block; set together with cursor_fg. #RRGGBB.
# cursor_bg = "#d0d0d0"
# Selection text; set together with selection_bg (unset: inverse video). #RRGGBB.
# selection_fg = "#121214"
# Selection ground; set together with selection_fg (unset: inverse video). #RRGGBB.
# selection_bg = "#d0d0d0"
# ANSI colours 0-15. #RRGGBB.
# ansi = ["#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5", "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff"]

[mux]
# Instance name used to derive pmux.sock / pmux-<instance>.sock. non-empty name, no '/'.
instance = "default"
# Absolute socket path; overrides instance when set. absolute path.
# socket = "/run/user/1000/prismattyc/pmux.sock"
# Attach in a TTY after pmux new. true|false.
attach_on_new = true
# Re-run saved pane commands on space open. agents|all|none.
space_open_runs_commands = "agents"
# Who sets pane size when a TTY attach shares the host window. latest|host.
remote_size = "latest"

[keys]
# Host actions. An entry replaces that action's default chords.
# Value is a chord string or an array of chord strings.
# split the focused pane to the right.
split_right = ['ctrl+shift+\', "ctrl+shift+e"]
# split the focused pane downward.
split_down = ["ctrl+shift+-", "ctrl+shift+d"]
# close the focused pane.
close_pane = "ctrl+shift+w"
# detach this session view (last tab exits).
detach = "ctrl+shift+x"
# focus the pane to the left.
focus_left = "alt+left"
# focus the pane to the right.
focus_right = "alt+right"
# focus the pane above.
focus_up = "alt+up"
# focus the pane below.
focus_down = "alt+down"
# cycle the focus border color forward.
focus_border_next = "ctrl+shift+]"
# cycle the focus border color back.
focus_border_prev = "ctrl+shift+["
# swap the focused pane with the previous pane.
# swap_pane_prev = []
# swap the focused pane with the next pane.
# swap_pane_next = []
# rotate every pane one slot forward.
# rotate_panes = []
# rotate every pane one slot back.
# rotate_panes_back = []
# focus the previously focused pane in this tab.
# focus_last_pane = []
# new tab.
new_tab = "ctrl+shift+t"
# new blank terminal tab.
new_blank_tab = "ctrl+alt+shift+t"
# new automatically named session tab.
new_session_tab = "ctrl+alt+shift+n"
# split right with a blank terminal.
blank_split_right = "ctrl+alt+shift+e"
# split down with a blank terminal.
blank_split_down = "ctrl+alt+shift+d"
# split right with an automatically named session.
session_split_right = "ctrl+alt+shift+r"
# split down with an automatically named session.
session_split_down = "ctrl+alt+shift+b"
# find a terminal across Spaces.
terminal_switcher = "ctrl+shift+o"
# view pending mail and pane input queue receipts.
# agent_messages = []
# update, restart components, and inspect versions.
# update_restart = []
# close the active tab.
close_tab = "ctrl+shift+q"
# rename the active tab.
rename_tab = "ctrl+shift+r"
# name the focused session and mailbox (local shells: pane title).
# rename_pane = []
# previous tab.
prev_tab = "ctrl+shift+pageup"
# next tab.
next_tab = "ctrl+shift+pagedown"
# select the previously active tab.
# last_tab = []
# select tab 1.
select_tab_1 = "ctrl+shift+1"
# select tab 2.
select_tab_2 = "ctrl+shift+2"
# select tab 3.
select_tab_3 = "ctrl+shift+3"
# select tab 4.
select_tab_4 = "ctrl+shift+4"
# select tab 5.
select_tab_5 = "ctrl+shift+5"
# select tab 6.
select_tab_6 = "ctrl+shift+6"
# select tab 7.
select_tab_7 = "ctrl+shift+7"
# select tab 8.
select_tab_8 = "ctrl+shift+8"
# select tab 9.
select_tab_9 = "ctrl+shift+9"
# move the focused pane to the previous tab.
move_pane_prev_tab = "ctrl+shift+alt+pageup"
# move the focused pane to the next tab.
move_pane_next_tab = "ctrl+shift+alt+pagedown"
# extract the focused pane into its own tab.
# break_pane = []
# join the focused pane into the previous tab.
# join_pane = []
# move the active tab one slot left.
# move_tab_left = []
# move the active tab one slot right.
# move_tab_right = []
# even 2-column layout.
layout_2 = ["ctrl+shift+f2", "ctrl+shift+alt+f2", "super+shift+f2", "ctrl+alt+2"]
# even 3-column layout.
layout_3 = ["ctrl+shift+f3", "ctrl+shift+alt+f3", "super+shift+f3", "ctrl+alt+3"]
# even 2×2 quadrant layout.
layout_4 = ["ctrl+shift+f4", "ctrl+shift+alt+f4", "super+shift+f4", "ctrl+alt+4"]
# even 5-column layout.
layout_5 = ["ctrl+shift+f5", "ctrl+shift+alt+f5", "super+shift+f5", "ctrl+alt+5"]
# even 6-column layout.
layout_6 = ["ctrl+shift+f6", "ctrl+shift+alt+f6", "super+shift+f6", "ctrl+alt+6"]
# even 7-column layout.
layout_7 = ["ctrl+shift+f7", "ctrl+shift+alt+f7", "super+shift+f7", "ctrl+alt+7"]
# even 8-column layout.
layout_8 = ["ctrl+shift+f8", "ctrl+shift+alt+f8", "super+shift+f8", "ctrl+alt+8"]
# even 9-column layout.
layout_9 = ["ctrl+shift+f9", "ctrl+shift+alt+f9", "super+shift+f9", "ctrl+alt+9"]
# retile to one pane when the tab has one pane; no-op otherwise.
# preset_single = []
# retile existing panes into even columns.
# preset_split_h = []
# retile existing panes into even rows.
# preset_split_v = []
# retile existing panes into an even two-row grid.
# preset_grid = []
# retile: focused pane on the left, others stacked on the right.
# preset_main_vertical = []
# retile: focused pane on top, others in a row below.
# preset_main_horizontal = []
# zoom the focused pane to the whole tab; again restores the split.
zoom_pane = "ctrl+shift+z"
# open a new OS window.
new_window = "super+n"
# edit the config file.
# open_config = []
# open the command palette.
command_palette = "ctrl+shift+p"
# next command-palette filter chip.
# palette_filter_next = []
# previous command-palette filter chip.
# palette_filter_prev = []
# open theme settings.
theme_picker = "ctrl+shift+,"
# open a saved space.
# open_space = []
# delete a saved space.
# delete_space = []
# move the focused pane to another saved space.
# move_pane_to_space = []
# focus the spaces rail.
# space_rail_focus = []
# choose rail position, autosave, and startup behavior.
# space_settings = []
# undo the last session removal or move.
# undo_space_change = []
# open the next saved space.
# space_rail_next = []
# open the previous saved space.
# space_rail_prev = []
# save the current space arrangement.
# save_space = []
# find in scrollback.
find = "ctrl+shift+f"
# open the walkthrough caption.
# walkthrough = []
# delete walkthrough progress and restart at level 0.
# walkthrough_reset = []
# copy the selection.
copy = "ctrl+shift+c"
# paste the clipboard.
paste = "ctrl+shift+v"
# select the visible viewport.
select_all = "ctrl+shift+a"
# scroll history up one line.
scroll_line_up = "ctrl+shift+up"
# scroll history down one line.
scroll_line_down = "ctrl+shift+down"
# toggle rich focus (--experimental-rich).
rich_focus = "ctrl+shift+g"

# -- accessibility --
[a11y]
# Expose host chrome through AccessKit (VoiceOver / Orca). true|false.
os_tree = true
# Speak mail, attention, pane-title notices, and cursor-line changes (PT-175). true|false.
announce = true
```

All spacing values accept `0` through `128` physical pixels.

### Accessibility

`[a11y]` exposes host chrome through AccessKit (VoiceOver on macOS, Orca on
Linux). Both keys default on when the table is absent.

| Key | Default | Effect |
|---|---|---|
| `os_tree` | `true` | Register the AccessKit adapter at window create. `false` leaves the window unchanged. Startup only. |
| `announce` | `true` | Live-region speech for mail, attention, pane-title notices, cursor line, and selection. `false` keeps the tree and silences speech. Hot-reloaded. |

The tree covers the window, tab strip, pane names, command palette, splash,
find, theme picker, space rail, and the scroll chip. The focused pane's
visible lines are one Document node (plain text, caret offset, host
selection). Unfocused panes stay name and badge only. Scrollback is not
in the live node.

The tree always includes a live-region node named `announce`. A silent
frame keeps that node with an empty value. Mail depth rises, attention
prompts, and unfocused-pane title notices speak first (assertive).
Selection complete or copy speaks next (polite). Cursor-line speech
fires on pane focus or row change and coalesces to 400 ms. Two identical
utterances toggle a trailing zero-width space so the value still
changes. Audio is never the only channel.

The host republishes the tree after each dirty paint. The `RedrawRequested`
path calls `publish_a11y` after present. The adapter's `update_if_active`
does nothing until a screen reader attaches. A compositor expose that is
not dirty does not rebuild the tree.

### Transparency

The following keys control how much shows through. The layers work from back to front:

1. the desktop and whatever windows sit behind Prismattyc;
2. the **window ground** — `pane_backdrop` behind each pane, `chrome_bg` in
   the gaps and window padding — painted at alpha `window_opacity`;
3. the **background image**, if `background_image` is set, with its own
   `background_opacity` tint and `background_blur_px`;
4. each **pane surface** — padding, structural outline, spare grid pixels, and
   default-background cells — scaled by `pane_opacity_active` or
   `pane_opacity_inactive`;
5. the **glyphs**, cursor, badges, and explicit SGR backgrounds, always opaque.

`pane_opacity_active` and `pane_opacity_inactive` scale the alpha of each
pane's padding, structural outline, spare grid pixels, and default-background
cells. Cell colors still blend toward `pane_backdrop` (or toward the background
image when one is set). Values above `0.8` are hard to see; **use `0.6`–`0.8`**
for a visible difference between the focused pane and the rest. `1.0` leaves
the pane at the window alpha.

`pane_backdrop` is a theme key. Every built-in derives one: dark themes reuse
`chrome_bg`, light themes take a 6% darker `default_bg`, and a theme whose
`chrome_bg` is nearly its `default_bg` falls back to a darkened `default_bg`.
A `themes/*.toml` file may set `pane_backdrop = "#RRGGBB"` to override it. It
hot-reloads with the theme.

`overlay_opacity` controls the tint weight of the frosted command palette,
context menus, space picker, save-space prompt, and theme picker. It accepts
`0.0` through `1.0`. Remove the key to follow `pane_opacity_active`. The
in-frame blur still follows `background_blur_px` and `window_blur`. The key
hot-reloads with the rest of the config.

`tab_active_bg` is a theme key (PT-95). The active tab chip fills with it.
Inactive chips keep `chrome_bg`. The default is `chrome_bg` blended toward
`chrome_fg` (8% on dark themes, 6% on light). A theme file may set
`tab_active_bg = "#RRGGBB"` to override it. It hot-reloads with the theme.
There is no `config.toml` per-colour theme-override table.

`hover_blend` controls immediate feedback on interactive chrome. It accepts
`0.0` through `0.3` and defaults to `0.10`. Dark themes blend the hovered
element toward `chrome_fg`; light themes use 80% of the value to darken it.
Hover composes over the current state, including `tab_active_bg`, and does
not animate. The key hot-reloads.

`window_opacity` below `1.0` makes the window ground and every default-
background cell translucent, so the desktop shows through — Ghostty's
`background-opacity`. Cells that carry an explicit SGR background stay opaque,
as do text, cursor, and badges. A dimmed pane is more translucent than the
focused one: its surface is painted at
`window_opacity × that pane's opacity`, so an inactive pane at `0.6` under a
`0.8` window shows more desktop than the active pane at `1.0`.

`chrome_opacity` sets the tab strip and the footer rail; it defaults to
`window_opacity`. The bars themselves become translucent; their text and
badges do not.

When several windows attach to the same session, a pane is sized to the
smallest attached window. A larger window shows that pane at the top left
and fills the rest of its frame with the window ground — at
`window_opacity`, so on an alpha-capable path the frame is see-through
rather than a solid border. This is intentional; attach windows of similar
size if the look bothers you.

#### Support matrix

| Session | Present path | `window_opacity` | `window_blur` |
| --- | --- | --- | --- |
| Native Wayland (KWin, Hyprland, wlroots) | own `wl_shm` `ARGB8888` present (PT-118) | works | KWin 6.7+: works via `ext-background-effect-v1`. Hyprland: use a `windowrulev2 = blur` instead; see [docs/hyprland.md](hyprland.md) |
| X11 with a compositor | softbuffer, depth-32 ARGB visual | works | ignored, with a startup notice |
| XWayland (`unset WAYLAND_DISPLAY`) | softbuffer, depth-32 ARGB visual | works | ignored, with a startup notice |
| macOS | alpha-capable Core Animation present | works for the window ground; text and explicit backgrounds stay opaque | works through an AppKit backdrop; hot-reloads without recreating the window |
| Windows | softbuffer | untested, treated as unsupported | ignored, with a startup notice |
| `--gpu` (`--features gpu`) | wgpu | ignored; the composite-alpha mode is not wired yet | ignored, with a startup notice |

On native Wayland the host bypasses softbuffer and presents its own
`wl_shm` `ARGB8888` buffers on the winit surface. ARGB8888 is one of the two
formats every Wayland compositor must accept, so the path is portable by
specification. softbuffer still serves X11, where its backend accepts
depth-32 visuals and a window created with an alpha visual carries
premultiplied ARGB. When no path can carry alpha the host prints one line at
startup and treats `window_opacity` as `1.0`; nothing else changes.

On macOS, the host uses an alpha-capable Core Animation surface. The
`window_opacity` value applies to the window ground. Text, the cursor, badges,
and explicit SGR backgrounds stay opaque. When `window_blur` is `true`, the
host adds an `NSVisualEffectView` behind the winit content view. It uses the
`underWindowBackground` material, `behindWindow` blending, and the active
state. Set `window_opacity` below `1.0` to make the backdrop visible. The host
adds or removes the effect view when you change the setting. The host keeps
the existing `background_image` and `background_blur_px` raster paths
unchanged.

`chrome_opacity` still needs a present path with per-pixel alpha, such as
macOS, native Wayland, or X11 with a depth-32 visual.

`window_blur` asks the compositor or AppKit to blur what is behind the
window. On KWin 6.7+ the host installs an `ext-background-effect-v1` blur for
the whole window when the key is `true`. Compositors without that protocol
(Hyprland and other wlroots compositors) apply blur compositor-side; the key
is then accepted and ignored with a startup notice.

The alpha visual is chosen once, when the window is created. On Wayland and
X11, raising or lowering `window_opacity` while the host runs applies on the
next frame only if the window already has an alpha visual; going from `1.0`
to anything lower needs a host restart, and the host says so on the next poll.
On macOS, the Core Animation present path already carries alpha. You can
change `window_opacity` while the host runs without recreating the window.

### Active tab and current space chip

The active tab chip (and the current chip in the spaces rail) is painted in a
variation of the focus colour: the focus colour blended 55 % over `chrome_bg`
on dark themes (45 % on light), toned further toward `chrome_bg` only as far
as needed for the title ink to reach WCAG AA (4.5:1). Ink is chosen for
contrast — light text on a dark chip, dark text on a light one — so the
selection reads at a glance whichever focus colour you pick. A theme file
that sets `tab_active_bg` pins the chip colour instead; the ink is still
chosen by contrast.

### Themes

Prismattyc ships 29 built-in themes:

| Config slug | Display name |
| --- | --- |
| `prismattyc-default` | Prismattyc Default |
| `catppuccin-mocha` | Catppuccin Mocha |
| `tokyo-night` | Tokyo Night |
| `rose-pine-moon` | Rosé Pine Moon |
| `monokai` | Monokai (classic) |
| `monokai-pro` | Monokai Pro (CE) (MIT, Monokai Pro Community Edition) |
| `omarchy-tokyo-night` | Omarchy Tokyo Night |
| `omarchy-osaka-jade` | Omarchy Osaka Jade |
| `omarchy-matte-black` | Omarchy Matte Black |
| `monokai-dimmed` | Dimmed Monokai (MIT, iTerm2-Color-Schemes) |
| `monokai-remastered` | Monokai Remastered (MIT, iTerm2-Color-Schemes) |
| `monokai-soda` | Monokai Soda (MIT, iTerm2-Color-Schemes) |
| `monokai-vivid` | Monokai Vivid (MIT, iTerm2-Color-Schemes) |
| `dracula` | Dracula |
| `ghost` | Ghost |
| `japanesque` | Japanesque (MIT, iTerm2-Color-Schemes) |
| `hive-monochromatic` | Hive Monochromatic |
| `hive-two-tone` | Hive Two-Tone |
| `hive-tri-tone` | Hive Tri-Tone |
| `hive-complementary` | Hive Complementary |
| `hive-split-complementary` | Hive Split-Complementary |
| `hive-analogous` | Hive Analogous |
| `hive-triadic` | Hive Triadic |
| `hive-high-contrast-dual` | Hive High-Contrast Dual |
| `hive-muted-professional` | Hive Muted Professional |
| `hive-night-hive` | Hive Night Hive |
| `hive-monochromatic-light` | Hive Monochromatic Light |
| `hive-tri-tone-light` | Hive Tri-Tone Light |
| `hive-muted-professional-light` | Hive Muted Professional Light |

The Omarchy themes use palettes from the official Omarchy repository, with
window-control colors mapped to Prismattyc. Their [MIT notice](../crates/prismattyc-host/themes/OMARCHY-LICENSE.txt)
is included with the themes.

Monokai Spectrum has been removed. If your configuration selected it, set
`theme = "omarchy-tokyo-night"` or choose another theme from the picker.

Slugs and display names are case-insensitive. A bare name first checks a
`themes/` directory next to `config.toml`, then the embedded themes. This lets
`~/.config/prismattyc/themes/rose-pine-moon.toml` intentionally override a built-in.
Explicit paths must be absolute so theme selection does not depend on the
directory that launched Prismattyc.

Theme files are TOML data with identity/source metadata, terminal defaults,
host chrome tokens, optional cursor/selection pairs, and exactly 16 ANSI
colors. The embedded files under `crates/prismattyc-host/themes/` are complete
examples. Theme selection changes Default and ANSI 0–15; guest RGB colors and
the stock xterm 16–255 cube/grayscale pass through unchanged. The seven-color
Prismattyc focus spectrum is brand chrome and also remains independent.

Press `Ctrl+Shift+,` in `prismattyc-host` to open the keyboard-only theme settings
surface. `Up` / `Down` (or `Home` / `End`) previews the built-in themes live,
including host chrome and an ANSI 0–15 strip. Families with two or more
variants (Monokai, Hive) show a trailing `>` on the root list. `Right` opens
that family. `Left` returns to the root. `PageUp` / `PageDown` scrolls when
the current list is taller than the window. `Enter` atomically updates the
`theme` key in `config.toml` while preserving its comments and other keys;
`Escape` restores the exact pre-picker theme. Custom file themes remain valid
config choices, but the picker lists the curated built-ins.

Use the `open_config` palette action to open `config.toml` in `$VISUAL`, then
`$EDITOR`, or `nano`. The action creates the default template only when the
file is missing. It does not parse the file before opening the editor, so you
can repair malformed TOML.

```toml
# Host keybindings (ADR-0015). Each entry replaces that action's default
# chords; list aliases explicitly; [] unbinds. Chord = mod+...+key with
# ctrl, shift, alt, super (cmd/meta/win) and a key: a letter, digit,
# punctuation (`\` `-` `=` `[` `]` `;` `'` `,` `.` `/` or spelled: backslash,
# minus, comma, ...), or enter, tab, backspace, delete, escape, space,
# insert, up, down, left, right, home, end, pageup, pagedown, f1-f24.
# A chord needs ctrl, alt, or super. Two actions on one chord, an unknown
# action, or a reserved input (plain ctrl+c, ctrl+2, ctrl+space,
# shift+insert, the ctrl+shift punctuation find fallbacks) reject the file.
# An active IME may consume ctrl+space or ctrl+2 before the host receives it.
# Use Shift+arrow or another available mark path when needed.
# Action names and defaults: `prismattyc-host --help`.
[keys]
split_right = "ctrl+alt+enter"
find = ["ctrl+shift+f", "ctrl+alt+f"]
command_palette = "ctrl+shift+p"
theme_picker = []
open_config = []
# Layout presets retile the current tab's existing panes. They do not spawn
# or close a pane. `layout_2` … `layout_9` still spawn until N panes.
# Presets are unbound by default. Bind them here if you want a chord.
# preset_single = []          # no-op when the tab has more than one pane
# preset_split_h = "ctrl+alt+h"  # even columns
# preset_split_v = "ctrl+alt+v"  # even rows
# preset_grid = "ctrl+alt+g"     # two rows; ceil(n/2) on top (3 panes → 2+1)
# preset_main_vertical = []      # focused pane left; others stacked right
# preset_main_horizontal = []    # focused pane top; others in a row below

# Mux defaults. prismattyc-host accepts this table and ignores it.
# pmux applies it; CLI flags and PMUX_* / PMUX_* still win.
[mux]
instance = "default"                 # $XDG_RUNTIME_DIR/prismattyc/pmux.sock
# socket = "/run/user/1000/prismattyc/pmux.sock"  # absolute; overrides instance
# attach_on_new = false              # default true: `pmux new` attaches in a TTY
# space_open_runs_commands = "agents"  # agents|all|none; `pmux space open --no-run` skips
# remote_size = "latest"               # latest|host; TTY attach fits, detach restores host
```

Layout presets (`preset_single`, `preset_split_h`, `preset_split_v`,
`preset_grid`, `preset_main_vertical`, `preset_main_horizontal`) retile
the current tab's existing panes. They do not spawn or close a pane.
That is the difference from `layout_2` … `layout_9`.
They are unbound by default. Bind them under `[keys]` if you want a chord.
`preset_single` is a no-op when the tab has more than one pane.
`preset_split_h` is even columns. `preset_split_v` is even rows.
`preset_grid` is two even rows with `ceil(n/2)` panes on top
(three panes become two plus one). `preset_main_vertical` puts the
focused pane on the left and stacks the others on the right.
`preset_main_horizontal` puts the focused pane on top and the others
in a row below. Zoom is cleared first, as for other retiles. A retile
that cannot satisfy pane minima leaves the layout unchanged and shows
a status message.

## Spaces rail

The rail shows the saved spaces (`spaces/*.json`, see `pmux space ls`) as
chips, the space counterpart of the tab strip. It is a view over the files:
a chip never owns a session.

Open the command palette and run `space_settings`. You can also right-click
a Space chip and select **Spaces settings…**. Changes are saved to this
configuration and apply to the running windows.

```toml
space_rail = "bottom"        # bottom (default) | left | top | right | off
space_rail_chip_cols = 0     # horizontal chip limit, 6-40; 0 = 28
space_rail_width_cols = 18   # side rail width in cells, 8-60
space_rail_pane_names = true # show live session names in a second row
```

- `bottom` / `top`: one row of chips under the panes (above the Ctrl+Shift
  chord strip) or above the tab strip. Chips start at the left edge; each
  chip is as wide as its label plus the close cell, up to
  `space_rail_chip_cols` (longer names ellipsize). A compact `+` chip
  follows the last space.
- `left` / `right`: a fixed-width column with chips stacked from the top.
  Drag the inner edge to resize it. The width is saved as
  `space_rail_width_cols`. A long name does not resize the terminal area.
- With `space_rail_pane_names = true`, each chip shows live session names in a
  second row at the terminal font size. Names update after a session moves or is renamed. A name disappears
  when its last live pane exits.
  Long lists are clipped inside the chip. Open its context menu for full names.
- The rail is part of the host geometry: pane content shrinks by the rail's
  row or column, and PTY sizes follow. Chips that do not fit are not shown.
- Chips are listed oldest first. Saving or renaming keeps their order. The
  current space sits on `tab_active_bg` (the same lift as the active tab)
  with a focus-colour title, a marker line, and a dot where the `×` would
  be. The host knows its space from `PMUX_SPACE`, a chip click, or `pmux
  space open` from a terminal; when it does not (a bare launch or `pmux
  attach --all`), it marks the one saved space whose tabs match the live
  tabs. The rail re-reads the directory once a second, so `pmux space
  save` / `rm` from a terminal update it.

Every click answers with a toast on the focused pane: "opening space
NAME", "NAME is the current space", or why nothing happened.

Mouse:

| Press | Effect |
|---|---|
| left click on a chip | switch to that space in this window (`pmux space open NAME --no-attach`); a fresh bare window is populated too |
| left click on `×` | arm the delete confirm on that chip; a second click, Enter, or Delete removes the file |
| right click on a chip | open the Space context menu, including Rename |
| middle click on a chip | arm the delete confirm |
| left click on `+` | a dialog asks for a new Space name; Enter creates one fresh shell and opens it in this window; Esc cancels |

Keyboard (palette actions, unbound by default; bind them under `[keys]`):

| Action | Effect |
|---|---|
| `space_rail_focus` | move focus to the rail: ←/→ (↑/↓ on a side rail) or Tab step over the chips and `+`, Enter opens (or creates a fresh Space on `+`), F2 renames, Delete asks, Esc returns to the pane |
| `space_rail_next` / `space_rail_prev` | open the neighbour of the current space, wrapping |
| `save_space` | save the current Space arrangement |
| `open_space` / `delete_space` | the filtered pickers over the same files |
| `move_pane_to_space` | move the focused session or blank terminal into another saved Space |

Each window has its own Space view. Opening a Space in a second window
leaves the first window unchanged. Windows on different Spaces use different
sessions. Move pane and Move session transfer running work to another Space.
The source view removes the transferred work. Moving its last session leaves
an empty view.

The current space cannot be deleted from the rail; open another space first.
A name that already exists, or one with `/` or `..`, keeps the dialog open
and shows why.

## Hot reload

The host polls the config file's mtime once a second (not inotify). Saving
the file applies changes on the next poll tick:

- `focus_border` — recolors immediately; deleting the key reverts to default.
- `theme` — swaps terminal defaults, ANSI 0–15, cursor/selection, and host
  chrome immediately; deleting the key restores Prismattyc Default.
- `focus_border_animation` — takes effect on the next focus change; switching
  to `"none"` (or deleting the key) settles any in-progress sweep.
- `focus_border_animation_ms` / `focus_border_animation_head` — apply from
  the next sweep; deleting a key restores its default (280ms / head on).
- `splash` — startup only; a reload does not show or hide an already-open
  splash. Default true. `--no-splash` and `PRISMATTYC_NO_SPLASH` still hide
  it when this key is true.
- `splash_animation` — applies immediately while the splash is showing;
  `false` freezes the art static, deleting the key restores the animation.
- `hover_blend` — applies immediately to strip, rail, and scrollbar hover;
  deleting the key restores `0.10`.
- `[keys]` — the key table is rebuilt on the next poll and the Ctrl+Shift
  chord strip relabels; deleting an entry restores that action's defaults.
  A conflicting or unparsable entry rejects the whole save (banner), so the
  previous bindings stay live.
- `background_image` / `background_opacity` / `background_blur_px` —
  re-decode on the next poll. Deleting `background_image` restores the
  flat theme background.
- `pane_opacity_active` / `pane_opacity_inactive` — repaint the pane surface
  (padding, outline, spare grid pixels, and default-background cells) on the
  next poll. Deleting a key restores 1.0.
- `window_opacity` / `chrome_opacity` — repaint on the next poll **when the
  window was created with an alpha visual**. Going from `1.0` to a lower value
  needs a restart; the host prints one line saying so. Deleting a key restores
  1.0 (opaque).
- `window_blur` — hot-reloaded. On macOS, the host adds an
  `NSVisualEffectView` behind the content view. It uses the
  `underWindowBackground` material with `behindWindow` blending. On native
  Wayland, it uses `ext-background-effect-v1`. It is accepted and ignored
  with a notice when the platform cannot provide blur. On Hyprland
  use a `windowrulev2 = blur` instead (see
  [docs/hyprland.md](hyprland.md)).
- `font` / `font_fallback` / `font_px` — reloads the font chain and recomputes
  the cell grid at the current window size.
- `font_ligatures` / `font_features` — update host-only terminal-grid shaping
  without changing PTY dimensions. The default is off; when enabled, the
  default features are `calt` and `liga`.
- `window_padding_px` / `pane_gap_px` / `pane_padding_px` — reflows pane
  slots, terminal content, and tab content immediately, including PTY sizes
  and hit-testing; deleting a key restores its 5px default.
- `space_rail_pane_names` — shows live session names in each chip. Set it to `false` to restore one-row chips.
- `space_rail` / `space_rail_chip_cols` — moves or hides the spaces rail and
  reflows the panes around it on the next poll; deleting a key restores
  `bottom` / 0 (28-cell cap).
- `pane_titles` — hot-reloaded. `focused` (default) shows the focused pane
  OSC title in the title row of a selected multi-pane tab. `hover` keeps
  the PT-148 handle-hover preview only. An unfocused pane whose title
  changes still lingers in the title row for `bell_toaster_ms` and tints
  its handle. Deleting the key restores `focused`.
- `drag_toaster` — applies to the next drag; deleting the key restores
  `true`.
- `visual_bell` / `audible_bell` / `bell_toaster` / `bell_toaster_ms` /
  `os_notify_bell` — apply to the next BEL; deleting a key restores its
  default (flash, sound, and toaster on, 10s linger; OS notification off).
  Reopening a log-backed session restores its output without replaying old
  bells or agent-attention alerts. New alerts still use these settings.
- `walkthrough_audio` — apply to the next walkthrough clip. Deleting the
  key restores `true`. Missing clips, `false`, or no player leave captions
  unchanged. `walkthrough_voice` is generation-time only
  (`scripts/walkthrough-voice.sh --voice`); playback uses bundled clips.
- `attention_sound` / `attention_badge` / `os_notify_attention` — apply to
  the next agent-attention signal; deleting a key restores sound, badge, and
  OS notifications. Attention OS notifications still throttle per pane.
  Turning `visual_bell` or `bell_toaster` off settles a lit flash or live
  BEL toast immediately. A write-fail chip
  (` input disconnected — reopen the pane `) stays until it expires or you
  click it.
- `[a11y] os_tree` — startup only. `false` skips AccessKit registration.
  The window looks and behaves as it did before this key existed. A reload
  that flips the key does not attach or detach the adapter; restart the
  host. Deleting the table restores the default (`true`). While the
  adapter is live, each dirty paint republishes the chrome tree.
- `[a11y] announce` — hot-reloaded. `false` keeps the AccessKit tree and
  silences live-region speech. Deleting the key restores the default
  (`true`).
- `panes` — startup only; a reload logs a reminder and changes nothing.

A save that fails parsing or validation is rejected as a whole: current
settings stay live, and the window shows a coral footer bar
(`config rejected: <reason>`) until the next valid save clears it. A config
that was already invalid at startup shows the same bar once the window opens.
The bar yields to the Ctrl+Shift chord cheat-sheet while held.

Live PTYs and scrollback are preserved by a reload. A font or spacing change
resizes the grid the same way a window resize does — which, like a window
resize, clears any in-progress selection.

Reloads only ever apply real, parseable file content: a briefly missing or
zero-length file (editors rename or truncate mid-save) is skipped. To reset
everything to defaults at runtime, save a file containing just a comment
(`# defaults`).

The poller watches `config.toml`, not a custom theme file. After editing a
custom theme, save `config.toml` again to load the new palette.

### Theme overrides

`[theme_overrides]` recolours keys of the named theme without copying a
theme file (PT-207). The template lists every key commented at the
prismattyc-default value; uncomment one to change it. Values are `#RRGGBB`.
`ansi` must list 16 colours. `cursor_fg`/`cursor_bg` and
`selection_fg`/`selection_bg` come in pairs. Overrides apply on top of any
theme, including one picked with `theme_picker`, and hot-reload with the
file. A bad value rejects the whole file with a stderr message, so the host
keeps its last good palette.

```toml
theme = "monokai"

[theme_overrides]
attention_badge = "#ff8800"
```

Unbound `[keys]` actions are listed commented (`# swap_pane_prev = []`) so
the action name is discoverable. Uncomment and set a chord to bind one.

## New panes and tabs

Choose a default in **Spaces settings → Session names**:

| Setting | New tabs, splits, and layout expansion |
|---|---|
| `session_naming = "ask"` (default) | Show the naming popup. Choose Create or Blank terminal. |
| `session_naming = "auto"` | Assign a suggested name and create a managed session. |
| `session_naming = "blank"` | Open a local login shell without a session or mailbox. |

The preference persists across launches. These direct actions override it:

| Action | Default shortcut | Result |
|---|---|---|
| `new_blank_tab` | Ctrl+Alt+Shift+T | New blank tab |
| `blank_split_right` | Ctrl+Alt+Shift+E | Blank split to the right |
| `blank_split_down` | Ctrl+Alt+Shift+D | Blank split below |
| `new_session_tab` | Ctrl+Alt+Shift+N | New automatically named session tab |
| `session_split_right` | Ctrl+Alt+Shift+R | Session split to the right |
| `session_split_down` | Ctrl+Alt+Shift+B | Session split below |

You can rebind these actions under `[keys]` or run them from the palette.
Blank terminals belong to the Space view where you open them. Switching
Spaces hides that view and keeps its shells running. Returning restores
its tabs, split layout, and focus.

To move a blank terminal, right-click its pane and select **Move pane to space**.
Select the destination Space. The terminal leaves the current view and
appears in the destination. Its process, directory, and scrollback stay intact.

### Restore blank terminals after restart

Enable **Spaces settings → Restore blanks**, or set
`restore_blank_terminals = true`. The default is `false`.
Prismattyc saves blank terminal tabs, split ratios, directories, and focus
for each Space view in this window. Reopening restores the active view.
Other views restore when you first open them.

Restoration opens fresh login shells in the saved directories. It does not
restore shell variables, running commands, or scrollback. If a directory
no longer exists, the shell starts in your home directory. Managed sessions
keep their existing processes. Closing the window ends its local shells.
Turning the setting off removes this window's saved blank terminal layout.

### Find an existing terminal

Press **Ctrl+Shift+O**, or run `terminal_switcher` from the palette.
Search by Space, session name, terminal title, or directory. Press Enter to
switch to the existing terminal. Blank terminals in this window and live
managed sessions appear together. A closed or moved target shows a message
instead of creating a replacement session.

### Space order and Git tab labels

Spaces appear oldest first. Saving or renaming a Space keeps its position.
For older Space files, Prismattyc uses the file creation time when available.
Otherwise, it uses the saved timestamp.

Each tab shows Git information for its focused pane when that pane is in a
local Git working tree. The label includes the repository and branch.
An asterisk (`*`) indicates working-tree changes. A detached HEAD shows the
short commit ID. The label refreshes in the background within a few seconds.
Narrow tabs prioritize the branch and dirty marker. Hover over the tab to
show its full title and Git information. Panes outside a Git working tree
keep their usual titles.
