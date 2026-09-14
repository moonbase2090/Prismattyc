# — TUI theme popularity and cleanroom notes

**Status:** Research baseline. implementation is documented in
[`config.md`](config.md); this file preserves the cleanroom rationale and
source trail.
**Ticket:** (child of).
**Date:** 2026-08-17.
**Rule:** Cleanroom. Peek at Ghostty structure and community names. Do not copy Ghostty or iTerm2-Color-Schemes files, converters, or hex tables.

Product freezes stay on the ticket. This file holds comparables, ranking, and structure notes.

## What Prism has today

`prismattyc-host` paints a hardcoded dark theme in `crates/prismattyc-host/src/raster.rs`:

| Token | Role |
| --- | --- |
| `DEFAULT_FG` / `DEFAULT_BG` | Cell default (`#d0d0d0` on `#121214`) |
| `CHROME_FG` / `CHROME_BG` | Host chrome |
| `PANE_BORDER` | Unfocused pane edge |
| `OVERLAY_BG` | z1 overlay fill |
| `UNSEEN_BADGE` / `MAIL_LETTER` / `ACTIVE_BADGE` | Status marks |
| `FOCUS_BORDER_PALETTE` | Brand spectrum (coral…ink). Config key `focus_border` only |
| ANSI 0–15 + xterm cube | Guest cell colors via `palette_rgb` |

No named palettes. No settings UI. Colors change only through the config file poller, and the only color key is `focus_border`.

A Prism theme is **host chrome + default cell colors + ANSI 0–15**. Guest TUIs (nvim, lazygit, k9s) keep their own schemes. Matching popular family names reduces chrome clash with those guests.

## Cleanroom boundary

**Looked at (structure and names only):**

- Ghostty docs: <https://ghostty.org/docs/features/theme>
- Ghostty config reference: `theme`, `background`, `foreground`, `palette`, `cursor-*`, `selection-*`, `window-theme`
- Ghostty `src/cli/list_themes.zig` UX surface (keys, panes, modes). Not transcribed as code.
- Ghostty `build.zig.zon`: themes come from `mbadolato/iTerm2-Color-Schemes`, not from Ghostty-authored palettes. Weekly vendor bump on main.
- Independent popularity: Dotfyle Neovim 2026, official family sites, 2026 editor/terminal comparisons, Ghostty community examples.

**Did not take:**

- Ghostty theme files or the iTerm2-Color-Schemes `ghostty/` dump
- Ghostty converters, `theme.zig` loaders, or `list_themes.zig` implementation
- Hex tables from Ghostty docs (their Catppuccin Frappé example is their file, not ours)
- Their preview sample text or TUI layout code

**When we implement, author palettes from official family publications** (MIT / Apache / public palette pages). Cite that source in the palette file header. Do not import a Ghostty or iTerm2 file and rename it.

**Shipped exception (operator, 2026-08-27):** builtin `ghost` / display name Ghost uses that peer's stock window defaults and ANSI 0–15 from its published MIT sources (`src/terminal/color.zig` `Name.default`, `src/config/Config.zig` `background` / `foreground`, inverted selection). Chrome tokens are Prismattyc-authored. This is not an iTerm2-Color-Schemes import.

Official palette sources for later:

- Catppuccin: <https://catppuccin.com/palette/>
- Rosé Pine: <https://rosepinetheme.com/>
- Tokyo Night: folke/tokyonight.nvim (and ports README)
- Dracula: <https://draculatheme.com/contribute> (spec)
- Monokai: classic public spec, plus `monokai-pro` from the Monokai Pro
  Community Edition (github.com/monokai-pro/{opencode,xfce4-terminal}, MIT,
  © 2025 Monokai) — the classic filter only. The other Monokai Pro filters
  (Machine, Octagon, Ristretto, Spectrum, Light) are not published under CE
  and are not shipped as such.
  `monokai-spectrum` is a Prism-authored mapping of publicly documented
  Filter Spectrum terminal colors, not the commercial product.
  `monokai-dimmed`, `monokai-remastered`, `monokai-soda`, `monokai-vivid`
  and `japanesque` are verbatim terminal colors from the iTerm2-Color-Schemes
  catalog (MIT) as redistributed by Ghostty 1.3.1 (MIT). That catalog's
  LICENSE keeps each theme's copyright with its original author and names
  no author for these five, so they ship under the catalog's MIT terms as
  received; the "Monokai Pro *" files in the same catalog are the commercial
  product and are not shipped.

## What Ghostty does (structure clues)

Ghostty does **not** invent popular names. It ships the iTerm2-Color-Schemes catalog (~450 files) and treats a theme as a named config fragment.

Observed patterns (generic; we can re-derive):

1. **Select by display name or path.** `theme = Catppuccin Frappe`. Absolute path allowed. No path separators in a name.
2. **Two lookup dirs.** User dir first (`$XDG_CONFIG_HOME/…/themes`), then shipped share dir. User overrides shipped.
3. **Theme file is data.** Typical keys: `background`, `foreground`, `cursor-color`, `cursor-text`, `selection-foreground`, `selection-background`, `palette = N=#RRGGBB` for 0–15 (optionally 0–255).
4. **Load order.** Theme first; user config overrides conflicting keys.
5. **Light/dark pair.** `theme = dark:Name,light:Name`. Follows OS appearance. Both sides required in that form.
6. **Title Case names** since Ghostty 1.2.0. Old slugs (`catppuccin-mocha`) broke. Display name = file name.
7. **Poster-child example in their docs is Catppuccin.** Light pair example uses Catppuccin Latte. Community snippets often use `dark:Rose Pine,light:Rose Pine Dawn`.
8. **Picker UX** (`ghostty +list-themes`):
   - TTY: interactive preview. Left list, right live sample (fg/bg/cursor/selection/16 colors).
   - Not a TTY / piped / `--plain`: print names.
   - Filter dark / light / all.
   - Fuzzy search. Copy name. Show the one config line to apply.
   - Optional write into an auto-include file.
9. **Chrome vs terminal.** Separate `window-theme` (`auto` / `system` / `light` / `dark`) so window chrome can follow the terminal background or the OS.
10. **They do not ship a settings GUI for themes yet.** Picker is a CLI TUI. Applying still means a config line.

**Prism takeaway:** steal the **information architecture** (named files, user-over-shipped, 16+fg/bg+cursor+selection, light/dark pair, live preview, Title Case names). Do not steal files or code. Do not ship 450 themes.

## Popularity ranking (TUI-weighted)

Best TUI proxy: Dotfyle “Top Neovim Colorschemes” 2026, ranked by installs across 1000+ tracked configs. Middle number below is config count.

| Rank | Family | Dotfyle configs | Why it matters for Prism |
| --- | --- | ---: | --- |
| 1 | **Catppuccin** | 974 | Soft pastel. Four flavors. Official ports for helix, lazygit, k9s, btop, yazi, tmux, bat. Ghostty docs lead with it. Mocha is the dark default. Latte is the usual light pair. |
| 2 | **Tokyo Night** | 887 | Moody blue-black. Strong syntax contrast. folke ports to Kitty/Alacritty/iTerm. Variants: Night, Storm, Moon, Day. |
| 3 | **Kanagawa** | 426 | Hokusai-inspired. Very common in TUI/Neovim, less often named in terminal-emulator marketing. |
| 4 | **Rosé Pine** | 352 | Ticket example (Moon). Warm “soho” palette. Variants: Main, Moon (darker), Dawn (light). Frequent Ghostty `dark:`/`light:` pair. |
| 5 | **Nightfox** | 271 | Family of variants (Nightfox, Nordfox, Terafox, …). Optional later. |
| 6 | **One Dark** | 215 | Atom classic. Still everywhere. |
| 7 | **Gruvbox Material** | 157 | Warm retro. Gruvbox family still a TUI staple. |
| 8 | **GitHub** | 156 | Familiar light/dark. Good for “looks like the website”. |
| 9 | **Everforest** | 128 | Low-contrast green. Long-session comfort. |
| — | **Dracula** | 93 | Lower Neovim rank, **widest cross-app ports**. High saturation. 2026 comparison articles still list it with Catppuccin and Tokyo Night as the three most-installed editor/terminal palettes. |
| — | **Nord** | 87 | Cool Arctic. Official ports across TUIs. |
| — | **Sonokai** | 80 | Monokai-Pro-like. Proxy for Monokai demand. |

Other signals that agree on the same short list:

- Ghostty docs example: Catppuccin. Community pair: Rosé Pine / Dawn.
- 2026 comparison writeups: Catppuccin (comfort), Tokyo Night (contrast), Dracula (punch + ports).
- Ticket owner examples: **Monokai**, **Rosé Pine Moon**.
- Cross-TUI port coverage (lazygit / k9s / btop / helix / yazi): Catppuccin, Rosé Pine, Tokyo Night, Dracula, Nord, Gruvbox.

**Not a popularity contest we should win with volume.** A mux host should ship a **small curated set** that (a) matches guest TUI families and (b) includes the two names the owner already asked for.

## Recommended ship set

### v1 (six dark + keep current)

| ID | Display name | Role |
| --- | --- | --- |
| `prism-default` | Prism Default | Current `#121214` host look. Always available. |
| `catppuccin-mocha` | Catppuccin Mocha | Most popular modern TUI dark. |
| `tokyo-night` | Tokyo Night | Second TUI dark. High contrast. |
| `rose-pine-moon` | Rosé Pine Moon | Owner request. |
| `monokai` | Monokai | Owner request. Classic, not Pro. |
| `dracula` | Dracula | Widest guest-app coverage. |

### v1 light pairs (same change, or immediately after)

| Dark | Light |
| --- | --- |
| Catppuccin Mocha | Catppuccin Latte |
| Rosé Pine Moon (or Main) | Rosé Pine Dawn |
| Tokyo Night | Tokyo Night Day |

Frappé / Macchiato are extra Catppuccin flavors. Nice later, not v1.

### v2 (only if v1 picker exists and people ask)

Kanagawa, Gruvbox Material, Nord or Everforest, One Dark, GitHub Dark/Light, Nightfox.

Do **not** vendor the 450-file iTerm2 catalog. That is Ghostty’s product choice, not ours.

## Suggested Prism data shape (re-derived, not copied)

A named palette file should cover **everything `raster.rs` currently hardcodes**, not only ANSI 16:

```text
name            Title Case display name
id              stable slug (catppuccin-mocha)
variant         dark | light
source          official palette URL + license
default_fg / default_bg
chrome_fg / chrome_bg
tab_active_bg          (optional; else chrome_bg blended toward chrome_fg)
pane_border
overlay_bg
unseen_badge / mail_letter / active_badge
cursor_fg / cursor_bg          (optional; else invert)
selection_fg / selection_bg    (optional; else invert)
ansi            16 RGB triples (0–15)
```

Keep `FOCUS_BORDER_PALETTE` as the **brand spectrum** unless a later slice lets a theme remap it. Mixing brand facets into every third-party palette will muddy the logo colors.

Config key (suggestion, not frozen):

```toml
theme = "rose-pine-moon"          # slug
# theme = "/abs/path/to/palette.toml"
# later: theme_dark / theme_light or theme = { dark = "...", light = "..." }
```

Lookup: user `$XDG_CONFIG_HOME/prism/themes/` first, then shipped. Unknown name → stay on Prism Default and log once.

Settings surface (from the ticket): **host chrome**, keyboard-reachable, live preview of chrome + a 16-color strip + a few sample cells. Not a 450-item marketplace. Not OS follow until named files exist.

## Implementation hygiene

1. One palette file per shipped theme. Human-readable TOML. Header cites official source and license.
2. Tests compare token maps, not screenshots.
3. Guest cells that already send RGB (truecolor) stay as sent. Theme only replaces Default + ANSI 0–15 + chrome tokens.
4. Do not regenerate the xterm 6×6×6 cube from the theme unless we later opt in. Many TUIs assume stock xterm 16–255.
5. Name in the UI is Title Case. Config accepts slug or Title Case.

## Sources

- Ghostty theme feature: <https://ghostty.org/docs/features/theme>
- Ghostty config `theme` / `palette`: <https://ghostty.org/docs/config/reference>
- iTerm2-Color-Schemes (Ghostty upstream catalog): <https://github.com/mbadolato/iTerm2-Color-Schemes>
- Ghostty 1.2.0 name break: iTerm2-Color-Schemes#608 (`catppuccin-mocha` → `Catppuccin Mocha`)
- Dotfyle Neovim 2026: <https://dotfyle.com/neovim/colorscheme/top>
- Catppuccin palette: <https://catppuccin.com/palette/>
- 2026 “most-installed” trio writeup (Catppuccin / Tokyo Night / Dracula): treated as secondary, not counted as a measurement
