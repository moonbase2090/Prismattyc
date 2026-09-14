# Mux parity and workspace-structure spike (PT-54)

**Status:** Spike. No protocol design. No tickets filed from this doc.
**Date:** 2026-08-28
**Lane:** docs only. Cite current verbs and paths where a row says "exists".

## Recommendation

Ship three follow-ups in this order:

1. Arrangement apply + swap/rotate/zoom on the existing split tree
   (`crates/prismattyc-mux/src/layout.rs`, `geometry.rs`).
2. A Space grouping of tabs inside a session. Do not add a fifth layer.
3. CLI send-keys, copy-mode search, and pane rename.

Do not invent a parallel layout engine. Do not treat zoom as a topology
mutation. ADR-0007 already says zoom is a view projection.

Long tables live in the appendices.

## 1. Command parity

### Recommendation

`pmux` already covers the daily attach/session/mail/sync/layout-save
loop. The gaps that hurt agent seats are **in-window layout edits**,
**named send-keys**, and **search in copy mode**. tmux remaining
strength is that surface, not a second multiplexer.

Status values: **exists**, **alias** (same job, other name), **gap**,
**size** (S/M/L).

### Top 10 (daily-driver for agent seats)

| Rank | Gap | Why | Size | Proposed ticket |
|------|-----|-----|------|-----------------|
| 1 | Zoom / maximize toggle | Full-screen TUI then restore. ADR-0007: view, not tree edit. | S | feat(mux): zoom pane as a client-local view |
| 2 | Swap / rotate panes | Fix a bad split without close. Tree edit in `layout.rs`. | S | feat(mux): swap and rotate panes in the split tree |
| 3 | Named arrangements (main-vertical / even / saved) | Host already has even `layout_2..9` (`keybind.rs` `Action::Layout`). CLI and attach do not. | M | feat(mux): apply named arrangements from palette and CLI |
| 4 | Send-keys to a pane by id | `pmux attach --write` is one-shot and takes a lease. Agents need `pmux send PANE TEXT`. | S | feat(mux): pmux send-keys to a pane id |
| 5 | Copy-mode search | Attach copy mode has motion and yank (`docs/mux-cli.md` Copy mode). No `/` search. Host has `Action::Find`. | M | feat(mux): search in pmux-attach copy mode |
| 6 | Break-pane / join-pane | Promote a pane to its own tab, or join it back. `MovePane` exists (ADR-0008). | M | feat(mux): break-pane and join-pane on MovePane |
| 7 | Last pane / last window | Jump back after a split. Client-local, like `SwitchWindow`. | S | feat(mux): last-pane and last-window |
| 8 | Pipe-pane / save-buffer | Evidence capture. `ReadPane` exists; no streaming log file. | M | closed — PT-116 [#174](https://github.com/brandanmajeske/Prismattyc/pull/174) |
| 9 | Rename pane | Windows rename via `RenameWindow`. Panes have `status-set` (PT-51), not a title. | S | feat(mux): rename pane |
| 10 | List-clients / detach-other | Operator hygiene. `pmux ls` shows viewers. No `detach -a`. `pmux kick` is SIGTERM of nested attach. | S | feat(mux): list-clients and detach-other |

Full row coverage is Appendix A.

### What already exists (do not reticket)

| Job | pmux today | Path / verb |
|-----|------------|-------------|
| Detach / reattach | `pmux attach`, `C-\ d` | `docs/mux-cli.md` |
| Split / close | host `split_right` / `split_down` / `close_pane`; control `Split` / `Close` | `keybind.rs`; ADR-0008 |
| Tabs | `Window` is the tab | ADR-0007; `docs/tabs-design.md` |
| New / close / rename tab | host `NewTab` / `CloseTab` / `RenameTab`; `CreateWindow` / `DestroyWindow` / `RenameWindow` | `keybind.rs`; ADR-0008 |
| Move pane across tabs | host `move_pane_prev_tab` / `move_pane_next_tab`; `MovePane` | `keybind.rs`; ADR-0008 |
| Even column / 2×2 layout | host `layout_2..9` → `even_horizontal_row` / `even_two_row_grid` | `crates/prismattyc-host/src/mux.rs`; `geometry.rs` |
| Saved trees | `pmux layout save` / `apply` / `ls` | `docs/mux-cli.md`; `layout_file.rs` |
| Sync input | `pmux sync on\|off`; attach `C-\ s` | `docs/mux-cli.md` Sync input |
| Guest chrome text | `pmux status-set` | PT-51; `docs/mux-cli.md` Status line |
| Shared attach | two attaches, `--read-only`, `[held]` | PT-52; `docs/mux-cli.md` Shared attach |
| Mail | `pmux mail *` | `docs/mux-cli.md` Mailbox verbs |
| Kick nested attach | `pmux kick` | `docs/mux-cli.md` |
| Copy / yank | attach copy mode + OSC 52 | `docs/mux-cli.md` Copy mode |
| Find in host | `Action::Find` | `keybind.rs` |
| Config keys | `[keys]` in `config.toml` | `docs/config.md`; ADR-0015 |

## 2. Workspace structure (spaces)

### Recommendation

Add **Space** as a named, ordered group of **windows (tabs)** inside a
**session**. Keep four levels. Do not replace session. Session stays the
agent-binding and mailbox unit (`pmux new NAME`, `$PMUX_AGENT`).

Do not make Space a second session. That would split mail and
`PRISMATTYC_PANE_ID` identity.

### User tasks

1. An agent operator juggles several repos in one seat. They want one
   chord to leave "hive-core tabs" and enter "prismattyc tabs".
2. A human watches two or three agent seats. Each seat is already a
   session (`grok-pc`, `fable-pc`). They need the tab strip, not a
   fourth switcher, until one seat grows many tabs.

### Design principles

1. One chord moves between contexts. A keystroke must not change
   meaning because a hidden space is active.
2. The focused pane stays visible in chrome: session, space, tab, pane.
3. Layout files round-trip spaces. A missing space field means one
   space named `main` that holds every window.

### Information architecture

| Level | Exists today | Owns | Does not own |
|-------|--------------|------|--------------|
| Session | yes (`Domain` / ADR-0007) | name, `agent_id`, mailbox, spawn env | focus |
| Space | **no** | title, ordered `WindowId`s, optional default layout name | PTY, agent id |
| Window (tab) | yes | title, split tree, `sync_input`, bounds | agent id |
| Pane | yes | PTY, lease, `status-set`, mail attention | tab order |

Focus and active tab stay client-local (`ClientView`, ADR-0007).
Active space is client-local too: `ClientView.space`.

### Options and trade-offs

| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| A. Space inside session | Matches iTerm/WezTerm workspaces. One mailbox. | Fourth name to teach. | **Choose this.** |
| B. Space = session | Zero domain work. | Switching "spaces" detaches mail identity. | Reject. |
| C. No Space; more tabs | Simplest. | Tab strip does not group 12 tabs. | Defer only if operators never exceed ~5 tabs/seat. |

### Chord and CLI

```bash
pmux space new NAME [--session S]
pmux space ls [SESSION]
pmux space switch NAME
pmux space move-tab [TAB] --to NAME
```

Host chords (ADR-0015 `[keys]` names, not bound yet):

| Action | Proposed name | Default idea |
|--------|---------------|--------------|
| Space switcher | `space_switcher` | Ctrl+Shift+O |
| Next / prev space | `next_space` / `prev_space` | Ctrl+Shift+} / { |
| New space | `new_space` | none (palette) |

Attach: `C-\ o` opens the same switcher as a toast list. Keep `C-\ d`
as detach.

### Migration

`SavedLayout` today (`layout_file.rs`):

```text
version, saved_at_unix, session, windows[{title, cols, rows, root}]
```

`SAVED_LAYOUT_VERSION` is `1`. A v2 file adds:

```text
spaces: [{ name, window_indexes: [0, 2] }]
```

Absent `spaces` reads as one space `main` with indexes `0..windows.len()`.
`pmux layout apply` creates windows in order, then groups them. Do not
rewrite v1 files on save until the operator opts in.

### Domain and control plane (impact only)

ADR-0007 IA grows one node: `Session → Space → Window → PaneLayout`.
New typed `SpaceId`, never reused. Empty-space policy: destroying the
last window in a space destroys that space; last space does not destroy
the session (unlike last window today).

ADR-0008 stays version `1`. Later additive verbs (not specified here):
`CreateSpace`, `SwitchSpace`, `MoveWindowToSpace`. Snapshot grows
`SessionSnapshot.spaces`. Old clients ignore unknown fields if serde
defaults hold.

### Wireframes

Host tab strip with spaces (low-fi):

```
+-- prismattyc-host --------------------------------------------+
| [hive]  [prismattyc*]  [mail]     session: grok-pc            |
|   tabs:  main | review | logs                                 |
| +-- pane A (focus) --------+-- pane B ----------------------+ |
| | $ cargo test             | | $ git log                    | |
| +--------------------------+--------------------------------+ |
| palette: space_switcher  Ctrl+Shift+O                         |
+---------------------------------------------------------------+
```

`*` is the active space. Tabs belong to that space only.

Attach chrome:

```
pmux-attach grok-pc / space prismattyc / tab review / pane 4
[scroll 12/40] │ build ok          [held]
C-\ d detach   C-\ o spaces        C-\ s sync
```

Switcher / palette flow:

```
+-- Command palette ----------------------------------+
| > space                                             |
|   Switch space: hive                                |
|   Switch space: prismattyc                          |
|   New space…                                        |
|   Apply arrangement: main-vertical                  |
|   Apply layout file: review.json                    |
+-----------------------------------------------------+
```

Type filters. Enter applies. Esc closes. The palette already exists
(PT-42, `Action::CommandPalette`).

## 3. Arrangement menu and repositioning

### Recommendation

Add a **named arrangement menu** on the command palette and
`pmux layout apply-arrangement NAME`. Rebuild the current window tree
with `even_horizontal_row`, `even_two_row_grid`, or a new
`main_vertical` / `main_horizontal` helper. Keep saved files on
`pmux layout apply`.

Add **swap**, **rotate**, and **ratio resize** as edits of the binary
split tree. Implement **zoom** as a client-local view (ADR-0007).
Implement **break** / **join** as `MovePane` wrappers.

Do not store a second layout graph.

### What maps onto the tree vs new primitives

| Operation | Mechanism | New primitive? |
|-----------|-----------|----------------|
| Even N-column, 2×2 | Rebuild with `even_horizontal_row` / `even_two_row_grid` (`geometry.rs`) | No. Host already does this (`MuxCommand::EvenColumns`). |
| Main-vertical / main-horizontal | New helper: one leaf + stacked rest | Small helper in `geometry.rs`. |
| Apply saved layout | `pmux layout apply` (`layout_file.rs`) | No. Window-scoped apply is the gap. |
| Swap with neighbor | Swap two `Leaf` ids in the tree | No. |
| Rotate | Cycle leaves in in-order walk | No. |
| Resize by cells / ratio | Change `Split.ratio`; control `Resize` already sets window bounds | Chord + divider drag are missing UX. |
| Zoom | Hide siblings in the client view | No topology change. |
| Break-pane | `MovePane` into a new `CreateWindow` | Wrapper. |
| Join-pane | `MovePane` into a target window | Wrapper. |
| Drag-and-drop in host | Hit-test slot + `MovePane` or swap | Host-only UX. |

### Menu wireframe

```
+-- Arrangements ------------------------------------+
| even-2     [ a | b ]                               |
| even-3     [ a | b | c ]                           |
| quadrants  [ a | b ]                               |
|            [ c | d ]                               |
| main-vert  [ MAIN | s1 ]                           |
|            [      | s2 ]                           |
| saved: review.json  (PT-49 file)                   |
+----------------------------------------------------+
| preview is ASCII; highlight matches current tree   |
+----------------------------------------------------+
```

Palette entries come from PT-42. Same list on `pmux layout ls-arrangements`.

### Chord and CLI

```bash
pmux layout ls-arrangements
pmux layout apply-arrangement NAME [--session S] [--window ID]
```

Host `[keys]` names (ADR-0015), unbound by default except existing
`layout_2..9`:

| Action | Name |
|--------|------|
| Swap with neighbor | `swap_pane_up` `swap_pane_down` `swap_pane_left` `swap_pane_right` |
| Rotate | `rotate_panes` |
| Zoom | `zoom_pane` |
| Break / join | `break_pane` `join_pane` |
| Resize | `resize_pane_left` `resize_pane_right` `resize_pane_up` `resize_pane_down` |
| Next arrangement | `next_layout` |

`pmux-attach` can offer swap/rotate/zoom/resize on `C-\` chords. It
cannot offer drag-and-drop. Divider drag belongs to `prismattyc-host`.

### Follow-up ticket titles (operator files after approval)

From §1:

- feat(mux): zoom pane as a client-local view
- feat(mux): swap and rotate panes in the split tree
- feat(mux): apply named arrangements from palette and CLI
- feat(mux): pmux send-keys to a pane id
- feat(mux): search in pmux-attach copy mode
- feat(mux): break-pane and join-pane on MovePane
- feat(mux): last-pane and last-window
- feat(mux): pipe-pane and save-buffer (closed, PT-116 #174)
- feat(mux): rename pane
- feat(mux): list-clients and detach-other

From §2:

- feat(mux): spaces group tabs inside a session
- feat(mux): space switcher chord and pmux space CLI
- feat(mux): layout file v2 optional spaces

From §3:

- feat(host): divider-drag pane resize
- feat(host): drag-and-drop pane onto a slot
- feat(mux): main-vertical and main-horizontal arrangements

Do not file these from this PR.

## Appendix A. Parity table

Columns: operation | tmux | zellij | screen | pmux | gap | size.

### Pane and window ops

| Operation | tmux | zellij | screen | pmux today | Gap | Size |
|-----------|------|--------|--------|------------|-----|------|
| Split | split-window | new pane | split | host split + `Split` | exists | — |
| Close | kill-pane | close | remove | host close + `Close` | exists | — |
| Swap neighbor | swap-pane -UDLR | move | — | none | gap | S |
| Rotate | rotate-window | — | — | none | gap | S |
| Break-pane | break-pane | — | — | `MovePane` + `CreateWindow`, no verb | alias/gap | M |
| Join-pane | join-pane | — | — | `MovePane`, no verb | alias/gap | M |
| Zoom | resize-pane -Z | fullscreen | — | none (ADR-0007: view) | gap | S |
| Resize cells | resize-pane -L/R/U/D | resize | resize | window `Resize` only | gap (pane ratio) | S |
| Even layouts | select-layout even-* | swap-layout | — | host `layout_2..9` | exists host / gap CLI+attach | S |
| Main-vertical | select-layout main-vertical | — | — | none | gap | S |
| Rename pane | select-pane -T | — | title | `status-set` is status, not title | gap | S |
| Rename window | rename-window | — | title | `RenameWindow`; host `RenameTab` | exists | — |
| Move pane to tab | move-pane -t | — | — | host `move_pane_*_tab`; `MovePane` | exists | — |

### Navigation

| Operation | tmux | zellij | screen | pmux today | Gap | Size |
|-----------|------|--------|--------|------------|-----|------|
| Focus by direction | select-pane -UDLR | move-focus | focus | host `focus_*` | exists host / gap attach | S |
| Select tab 1..9 | select-window -t | — | — | host `select_tab_N` | exists host | — |
| Last window | last-window | — | other | none | gap | S |
| Last pane | last-pane | — | — | none | gap | S |
| Find window | find-window | — | — | none | gap | M |
| Next/prev tab | next-window | — | next | host `next_tab` / `prev_tab` | exists host | — |

### Session ops

| Operation | tmux | zellij | screen | pmux today | Gap | Size |
|-----------|------|--------|--------|------------|-----|------|
| New session | new-session | — | screen | `pmux new` | exists | — |
| Attach | attach | — | attach | `pmux attach` | exists | — |
| List | ls | — | -list | `pmux ls` | exists | — |
| Rename session | rename-session | — | — | none (session name is create-time) | gap | S |
| Kill session | kill-session | — | quit | `pmux stop SESSION` | exists | — |
| Kill server | kill-server | — | — | `pmux stop` → `ShutdownServer` | exists | — |
| Session groups | — | — | — | none; propose Space instead | gap | L |

### Scrollback and logging

| Operation | tmux | zellij | screen | pmux today | Gap | Size |
|-----------|------|--------|--------|------------|-----|------|
| Copy mode | copy-mode | scroll | copy | attach `C-\ [` | exists | — |
| Search in copy | copy-mode / | — | — | host `Find`; attach none | gap attach | M |
| Save buffer | save-buffer | — | hardcopy | `pmux save-buffer` (PT-116 #174) | closed | — |
| Pipe pane | pipe-pane | — | log | `pmux pipe-pane` (PT-116 #174) | closed | — |
| Clear history | clear-history | — | — | none | gap | S |

### Input and automation

| Operation | tmux | zellij | screen | pmux today | Gap | Size |
|-----------|------|--------|--------|------------|-----|------|
| Send-keys | send-keys | — | stuff | `attach --write`; no pane-id CLI | gap | S |
| Sync panes | synchronize-panes | — | — | `pmux sync` (window) | exists | — |
| Paste stack | paste-buffer | — | — | host paste; no stack | gap | M |
| Respawn pane | respawn-pane | — | — | none | gap | M |
| run-shell / if-shell | yes | — | — | none | gap | L |
| Hooks | set-hook | — | — | none; mail inject is the agent path | gap | L |
| wait-for | wait-for | — | — | `pmux mail watch` is mail-only | gap | M |

### Status, clients, config

| Operation | tmux | zellij | screen | pmux today | Gap | Size |
|-----------|------|--------|--------|------------|-----|------|
| Status formats | status-left/right | — | hardstatus | attach chips + `status-set` | alias | — |
| Activity / silence / bell | monitor-* | — | monitor | mail attention; no silence | gap | M |
| List clients | list-clients | — | — | `pmux ls` viewers | alias | S |
| Detach other | detach-client -a | — | — | `pmux kick` (nested only) | gap | S |
| Lock | lock-session | — | lockscreen | none | gap | L |
| source-file | source-file | — | — | host `config.toml` hot reload | exists host | — |
| Key tables | bind-key -T | — | — | `[keys]` one table | exists host | — |
| set-option | set-option | — | — | config.toml + env | exists host | — |

## Appendix B. Arrangement and repositioning ops

| Operation | tmux | zellij | host today | attach today | Gap | Size |
|-----------|------|--------|------------|--------------|-----|------|
| Even N-col | select-layout even-horizontal | swap-layout | `layout_2..9` | none | CLI+attach | S |
| Quadrants | tiled | — | `layout_4` | none | CLI+attach | S |
| Main-vertical | main-vertical | — | none | none | gap | S |
| Next layout | next-layout | — | none | none | gap | S |
| Saved arrangement | none (scripts) | layout files | `pmux layout apply` (session) | none | window-scoped apply | S |
| Swap neighbor | swap-pane | move | none | none | gap | S |
| Rotate | rotate-window | — | none | none | gap | S |
| Zoom | resize-pane -Z | fullscreen | none | none | gap | S |
| Break / join | break-pane / join-pane | — | `MovePane` via tab move | none | wrapper CLI | M |
| Resize chord | resize-pane | resize | none | none | gap | S |
| Divider drag | mouse | mouse | scrollbar drag only | n/a | gap host | M |
| DnD pane | — | — | none | n/a | gap host | L |
