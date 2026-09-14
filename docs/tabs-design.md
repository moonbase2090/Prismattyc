# Tabs (frames) design note — spike

**Status:** Spike conclusion, 2026-08-13. Implementation tracked in the work breakdown below.
**Depends on:** PRD §2.8 (frozen); ADR-0007; ADR-0008; ADR-0010.

## Question

Add "tabs" (frames): group panes into tabs, create panes in a tab, move
existing panes between tabs. Is a tab a new grouping layer, or does the
existing model already contain it?

## Conclusion: the existing `Window` **is** the tab

No new grouping layer. Three frozen artifacts already name it:

- PRD §2.8 IA: `Window (tab)` (PRD.md:269); 2A scope line ships "sessions,
  windows (tabs), pane splits…" (PRD.md:257).
- ADR-0007: `Window (tab, opaque WindowId)`.
- `prismattyc-mux/src/ids.rs`: "Window identity (tab within a session)".

The domain layer supports N windows per session end-to-end today:
`Domain::create_window` / `destroy_window` (with the named empty-session
policy), ordered `Session.windows`, `snapshot()` emitting one
`WindowSnapshot` per window, and a passing multi-window test. `ClientView`
already models the active tab and per-window focus **client-locally**
(`ClientView.window`, `pane_focus: HashMap<WindowId, PaneId>`), which
answers the hardest question — where "active tab" lives — the PRD-compliant
way (§2.8.3: focus is client-owned; no server-side active-window state).

What is actually missing:

1. **Wire verbs** — no `CreateWindow` / `DestroyWindow` / `MovePane` /
   `SwitchWindow` in `ControlRequest`; `destroy_window` is unreachable from
   the wire today.
2. **Host runtime** — `MuxRuntime.window` is a scalar set once from
   `windows.first()`; the windowed host cannot represent a second tab.
3. **Chrome** — ADR-0010 deliberately scoped tabs out ("deferred until a
   multi-window runtime exists"). No tab bar, no tab chords.

## MovePane semantics (the one genuinely new mutation)

PRD mandates **swap/move preserves pane identity** (PRD.md:308, §2.8.12
proof gate). Therefore:

- `Domain::move_pane` is a two-window atomic mutation built from the
  existing geometry primitives (`close_pane_in_layout` on the source +
  `split_leaf` on the destination). Both layouts are cloned and probed
  against their own bounds before either `set_layout` commits; any failure
  rolls both back. The moved pane's `PaneId`, `PtySession`, `Emulator`, and
  controller lease are untouched — never close+respawn.
- On the wire it is **exactly one event** (`PaneMoved` carrying both window
  ids and a two-window geometry vec), never a `PaneClosed`+`PaneSplit`
  pair — replaying a pair would tear down and respawn the client's view of
  the pane, violating identity preservation.
- `LiveRuntime` needs no change: it is pane-keyed and window-agnostic, so
  PTY/emulator lifetime is naturally preserved across the move.
- Authorization follows the `Split`/`Close` optional-lease tier.
- Source-window collapse follows the existing close policy (empty window
  dies; last window ends the session only under the named policy).

## Host tab bar and chords

- Tab strip visibility follows `tab_strip = "auto" | "always" | "multi"` in the windowed host. The default `auto` mode shows the strip when `tab_count > 1` or the active tab has more than one pane. `always` forces the strip on. `multi` keeps the pre-PT-78 multiple-tab rule. A live tab or pane drag keeps the strip visible so the empty end remains a drop target.
- Chords (all currently unclaimed): `Ctrl+Shift+T` new tab,
  `Ctrl+Shift+PageUp/PageDown` prev/next, `Ctrl+Shift+Digit1..9` select,
  plus a move-pane-to-tab chord. Indexes are presentation-only and resolve
  to `WindowId` at press time (PRD.md:274).
- Active tab is marked by shape **and** the focus-border spectrum color,
  never color alone; no idle animation (PRD.md:328-329).
- Inactive tabs keep draining PTYs so unseen badges accumulate.

## Work breakdown

| Order | Layer | Scope |
|-------|-------|-------|
| 1 | `prismattyc-mux` domain | `Domain::move_pane` two-window atomic mutation + tests |
| 2 | `prismattyc-mux` control plane | `CreateWindow`/`DestroyWindow`/`MovePane`/`SwitchWindow` verbs, single `PaneMoved` event, ADR-0008 amendment (blocked by 1) |
| 3 | `prismattyc-host` runtime | multi-window `MuxRuntime` + tab chords (blocked by 1; independent of 2 — the host runs its own in-process `Domain`) |
| 4 | `prismattyc-host` chrome + docs | tab bar strip + ADR-0012 superseding ADR-0010's tab out-of-scope (blocked by 3) |

## Rejected alternative: a new "frame" layer above/below Window

Zero modeling gain over the existing window layer, contradicts the frozen
PRD §2.8 IA (would need a PRD amendment), and duplicates session/window
policy questions (empty-collapse, naming, ordering) that already have
frozen answers.
