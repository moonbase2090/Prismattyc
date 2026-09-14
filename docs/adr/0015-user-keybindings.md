# ADR-0015 — User keybindings for host actions

**Status:** Accepted (2026-08-27) · **Ticket:** PT-41 · **Amends:** ADR-0001 (D-H3/D-H4
rebinding note), ADR-0010 (out-of-scope bullet) · **Consumers:** PT-42 command palette

## Context

Every host chord is hard-coded: `mux_command`, `is_find_chord`, `is_theme_picker_chord`,
`is_copy_chord`, `is_paste_chord`, `is_select_all_chord`, `is_new_window_chord`, and the
Ctrl+Shift+Up/Down scroll keys each match winit events directly. The chord strip,
`--help`, the operator card, ADR-0010 and ADR-0012 carry hand-written copies of the same
labels. Users cannot change a chord that their compositor or outer terminal steals, and
PT-42 (command palette) needs named actions to bind through.

ADR-0001 D10 freezes selection and copy/paste key ownership and requires an ADR for new
chords. ADR-0010 lists "remappable key profiles" as out of scope. This ADR supersedes
that bullet and states which ADR-0001 inputs stay fixed.

## Decision

### D-K1 — Named actions are the single source

The host owns one action table (`keybind::Action`). Dispatch, the chord strip, `--help`,
and `docs/config.md` derive their chord labels from the same table. A chord is never
matched outside the table except for the fixed inputs in D-K3.

| Action | Default chord(s) | Origin |
|---|---|---|
| `split_right` | `ctrl+shift+\`, `ctrl+shift+e` | ADR-0010 |
| `split_down` | `ctrl+shift+-`, `ctrl+shift+d` | ADR-0010 |
| `close_pane` | `ctrl+shift+w` | ADR-0010 |
| `detach` | `ctrl+shift+x` | ADR-0010 |
| `focus_left` / `focus_right` / `focus_up` / `focus_down` | `alt+left` / `alt+right` / `alt+up` / `alt+down` | ADR-0010 |
| `focus_border_next` / `focus_border_prev` | `ctrl+shift+]` / `ctrl+shift+[` | ADR-0010 |
| `focus_last_pane` | unbound | PT-127 |
| `new_tab` / `close_tab` / `rename_tab` | `ctrl+shift+t` / `ctrl+shift+q` / `ctrl+shift+r` | ADR-0012 |
| `rename_pane` | unbound | PT-148 |
| `prev_tab` / `next_tab` | `ctrl+shift+pageup` / `ctrl+shift+pagedown` | ADR-0012 |
| `last_tab` | unbound | PT-127 |
| `select_tab_1` … `select_tab_9` | `ctrl+shift+1` … `ctrl+shift+9` | ADR-0012 |
| `move_pane_prev_tab` / `move_pane_next_tab` | `ctrl+shift+alt+pageup` / `ctrl+shift+alt+pagedown` | ADR-0012 |
| `move_tab_left` / `move_tab_right` | unbound | PT-69 |
| `layout_2` … `layout_9` | `ctrl+shift+f2` … `ctrl+shift+f9`; aliases `ctrl+shift+alt+fN` (Alt co-held), and for macOS `shift+super+fN` and `ctrl+alt+2…9` | ADR-0010 |
| `preset_single` / `preset_split_h` / `preset_split_v` / `preset_grid` | unbound | PT-70 |
| `preset_main_vertical` / `preset_main_horizontal` | unbound | PT-132 |
| `zoom_pane` | `ctrl+shift+z` | PT-57 |
| `new_window` | `super+n` | ADR-0012 |
| `open_config` | unbound | PT-179 |
| `command_palette` | `ctrl+shift+p` | PT-42 |
| `theme_picker` | `ctrl+shift+,` | config.md |
| `move_pane_to_space` | unbound | PT-182 |
| `find` | `ctrl+shift+f` | PT-37 |
| `copy` | `ctrl+shift+c` | ADR-0001 D-H4 |
| `paste` | `ctrl+shift+v` | ADR-0001 D-H3 |
| `select_all` | `ctrl+shift+a` | ADR-0001 D-H3 |
| `scroll_line_up` / `scroll_line_down` | `ctrl+shift+up` / `ctrl+shift+down` | host |
| `rich_focus` | `ctrl+shift+g` (only with `--experimental-rich`) | ADR-0013 |

Defaults reproduce the chords shipped before this ADR exactly, including the macOS
alternates for `layout_N` (macOS steals Ctrl+F2…F8).

### D-K2 — Config shape and chord grammar

```toml
[keys]
split_right = "ctrl+shift+backslash"        # one chord
find        = ["ctrl+shift+f", "ctrl+alt+f"] # aliases
theme_picker = []                            # unbind (fixed fallbacks stay)
```

- Table key = action name from D-K1. Value = one chord string or an array of chord
  strings. An empty array removes every default chord for that action.
- Chord grammar: `mod+…+key`, case-insensitive, whitespace ignored. Modifiers: `ctrl`
  (`control`), `shift`, `alt` (`option`), `super` (`cmd`, `meta`, `win`). Key: one printable
  character (`a`–`z`, `0`–`9`, `` ` - = [ ] \ ; ' , . / ``) or a name: `enter`, `tab`,
  `backspace`, `delete`, `escape`/`esc`, `space`, `insert`, `up`, `down`, `left`, `right`,
  `home`, `end`, `pageup`/`pgup`, `pagedown`/`pgdn`, `f1`–`f24`. Spelled punctuation is
  accepted too: `backslash`, `minus`, `equal`, `bracketleft`, `bracketright`, `semicolon`,
  `quote`, `comma`, `period`, `slash`, `backquote`.
- A chord must include `ctrl`, `alt`, or `super`. Plain and Shift-only keys belong to the
  PTY (ADR-0001 D-H3 "other keys"); a binding that would take them is rejected.
- Matching: `ctrl`, `shift`, `alt` must match exactly. `super` is required only when the
  chord names it and is otherwise ignored (some compositors leave Super sticky; ADR-0010
  matching already tolerates it). Character keys match the **physical scancode first**
  (layout-independent, as ADR-0010 requires; `\` also matches `IntlBackslash`, `-` also
  matches the numpad minus) and then the logical character case-insensitively, including
  the shifted glyph on the same key (`\`/`|`, `-`/`_`, `,`/`<`). Named keys match
  `NamedKey`; F-keys also match the macOS Fn representation.

### D-K3 — Fixed inputs (not actions, not rebindable)

These stay hard-coded because they are semantics or fallbacks, not chords a user picks:

- ADR-0001 D-H3 selection ownership: Shift+motion, motion in keyboard-select mode,
  `Ctrl+2` and `Ctrl+Space` mark, `Esc` clear, and **plain `Ctrl+C` with a multi-cell
  selection** (D-H4). Rebinding `copy` changes only the `Ctrl+Shift+C` chord.
- Fallbacks that exist because outer terminals steal chords (ADR-0001 "fallbacks stay
  first-class"): `Shift+Insert` paste; the `find` punctuation aliases
  (`Ctrl+Shift` + `/ ? ; : ' " . >`) and bare `/` while scrolled into history.
- Mode-internal keys: find, rename, theme picker, and splash navigation.
- macOS AppKit menu accelerators (`⌘N`, `⌘Q`); the menu is not driven by the table.

A user chord equal to a fixed input is a load error (D-K4).

### D-K4 — Conflict rule: error on load

The file is rejected as a whole (startup: "ignoring config" and defaults; hot reload:
the existing coral error banner, previous config stays live) when `[keys]` has:

- an unknown action name;
- a chord that does not parse, or lacks `ctrl`/`alt`/`super`;
- two actions whose effective chord sets overlap — user chords against user chords, and
  user chords against defaults that were not overridden — the error names both actions
  and the chord;
- a user chord equal to a fixed input from D-K3.

"Last wins" was rejected: it hides typos, and the existing tests already assert
non-collision (`mux_command` returns `None` for the find and theme-picker chords). A bad
chord never panics the host; parsing is total.

### D-K5 — Replacement, not layering

A user entry replaces every default chord of that action. Aliases must be listed
explicitly. Fixed fallbacks (D-K3) are untouched by any entry.

### D-K6 — Hot reload and labels

`[keys]` applies on the next config poll like the other keys. The chord strip re-renders
with the new labels; `--help` prints the defaults and points at `[keys]`.

## Consequences

- `crates/prismattyc-host/src/keybind.rs` holds `Action`, `Chord` (parse + match + label),
  the default table, and `KeyMap` validation. `main.rs` predicates become lookups.
- Tests that pin the default chords keep passing against `KeyMap::default()`; new tests
  cover the grammar, physical/logical matching, each D-K4 error, unbind, hot reload, and
  a user chord firing its action end to end.
- ADR-0010 "remappable key profiles" is no longer out of scope for rebinding; a leader or
  modal profile is still out of scope. PT-42 binds `command_palette` through this table.
- Operator card and ADR chord tables describe defaults; the config file is the override.

## Out of scope

- Rebinding PTY application keys (vim, readline) as host actions.
- Leader keys, modes, or profiles; per-pane bindings.
- Driving the macOS menu from the table.
