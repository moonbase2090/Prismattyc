# Phase 2 multiplexer — competitive research notes

**Status:** Living research supporting **[PRD.md §2.8](PRD.md)** (v0.6+).
**Not** an implementation plan until owner authorizes Phase 2 after A-6.
**Marker:** git tag `Pre-Mux`; Phase 1.5 tip `14b4e8b`+.

Product freezes live in the PRD. This file holds **comparables and citations**.
Where this doc once left questions open, §2.8 is authoritative.

## Goal (charter-aligned)

First-class **sessions / windows / panes** inside Prism’s **own host**
(`prismattyc-host` primary), with classic fidelity remaining the hard gate. Long-term
visual bar: **browser-sharp** chrome and motion **without** embedding a browser
engine (charter non-goal).

## Comparables (primary sources)

### tmux (reference model)

| Concept | Meaning |
|---------|---------|
| **Server** | Long-lived process owns PTYs |
| **Session** | Named collection of windows; detach/reattach |
| **Window** | Full-screen layout of panes |
| **Pane** | One PTY + scrollback + focus |
| **Layout** | Tree/split geometry; cycle presets |
| **Client** | Attach view; multiple clients possible |

**Strengths:** Detach durability, scriptability, control mode, remote SSH workflows.

**Weaknesses:** Nested terminal semantics, prefix-key tax, limited modern chrome.

**Primary links:**

- Getting Started (sessions/windows/panes): https://github.com/tmux/tmux/wiki/Getting-Started
- Control mode / machine protocol: https://github.com/tmux/tmux/wiki/Control-Mode
- Manual page overview: https://man7.org/linux/man-pages/man1/tmux.1.html

**Prism takeaway:** Steal the **information architecture**, not nested-in-another-emulator posture. Stable IDs and flow control inform the Phase 2 control plane.

### GNU screen / abduco + dvtm

- **screen:** Historical detach + windows; weaker pane model.
- **abduco:** Session persistence without layout — useful separation of **detach** vs **tiling**: https://github.com/martanne/abduco

### Zellij

Modern Rust mux: discoverable UI, floating panes, layouts, plugins.

**Primary links:**

- Session resurrection (safety of restore): https://zellij.dev/documentation/session-resurrection.html
- Session manager: https://zellij.dev/documentation/session-manager-alias.html

**Prism takeaway:** Discoverable chrome; cold restore must not auto-rerun commands without confirmation.

### WezTerm multiplexing

Built-in tabs/splits + optional mux domains — emulator owns mux (closest cousin).

**Primary link:** https://wezterm.org/multiplexing.html

**Prism takeaway:** Built-in mux is correct; users still want durable sessions or they keep tmux.

### Herdr (local product context)

Terminal workspace manager for AI coding agents: sessions, panes, agents, socket API.

**Primary link:** Socket / snapshot / pane ops: https://herdr.dev/docs/socket-api/

**Prism takeaway:** Interop via **generic mux primitives** only; do not become an agent IDE (charter). Study pane chrome density and attach ergonomics.

### Hive (related local)

Multi-pane operator TUI; Termwright-tested. Useful for **E2E patterns**, not VT ownership.

## Design principles (→ PRD §2.8)

1. One VT brain per pane — reuse emulator + `Screen`.
2. Host owns chrome — hybrid freeze z-order.
3. Windowed first — `prismattyc-host` product compositor.
4. **Detach is part of Phase 2 product claim** (2B); 2A alone is not “mux complete.”
5. Beauty without Electron — software→GPU ladder (D3/D9).
6. Scriptability — control plane in Phase 2 (Unix socket), not “later forever.”

## Suggested ticket budget (not filed)

~1 epic + 8–10 tasks for 2A; +2–4 for 2B → ~9–14 total. Zero tickets until owner authorizes.

## Frozen vs still open

| Topic | Authority |
|-------|-----------|
| Detach in Phase 2 claim | **Frozen** PRD §2.8.1 (2B required for claim) |
| Controller lease grain | **Frozen** PRD §2.8.4 (**per pane**) |
| Nested `prism` mux | Open (default: single-pane harness) |
| Exact key chords / palette | Open |
| Last-window/session end policy | Open |
| Remote transport | **Later** (not 2B) |
