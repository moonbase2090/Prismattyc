# Prismattyc – Initial Development Starting Points

**Classic terminal. Modern surface.**

These are the recommended first pieces of work when beginning development on Prismattyc.

---

## 1. Project Skeleton / Cargo Workspace Layout

Create the initial Rust workspace structure:

- Root `Cargo.toml` (workspace)
- `prism` – main binary crate
- `prismattyc-core` – shared types, protocol, screen model
- `prismattyc-emulator` – VT/xterm parsing + PTY handling
- `prismattyc-mux` – session / window / pane management
- `prismattyc-render` – hybrid cell grid + rich layer rendering
- `prismattyc-protocol` – capability queries + rich markup/canvas protocol definitions

Decide early on:
- Edition, MSRV, license
- Feature flags (e.g. `gpu`, `wayland`, `x11`)
- Basic CI (check, test, clippy)

---

## 2. High-level Architecture Diagram

Sketch (and later formalize) the major components and data flow:

- PTY / process layer
- Escape sequence parser → Screen model (classic cell grid + scrollback)
- Multiplexer (sessions, windows, panes, layouts)
- Renderer (classic path + optional rich/markup/canvas path)
- Client ↔ server control protocol (for detach/reattach and remote)
- Capability / feature discovery mechanism

Goal: a clear picture of how classic fidelity and opt-in richness coexist without fighting.

---

## 3. First Cut of the Capability / Query Protocol

Design the initial version of how applications discover Prismattyc’s rich features.

Requirements:
- Queryable (so programs can gracefully degrade)
- Versioned
- Extensible for future markup, styling, animation, and canvas features
- Lives cleanly alongside existing VT/Kitty/Sixel graphics protocols

Deliverable: a small design doc + initial Rust types / escape sequence definitions.

---

## 4. Hybrid Rendering Model Sketch

Define how the two layers interact:

- Classic cell grid (must remain fast and correct)
- Optional retained / markup / canvas layer
- Z-order, cursor ownership, selection, mouse, and input focus rules
- How rich content is attached to or overlaid on the cell grid

This is one of the highest-leverage design decisions in the whole project.

---

## 5. Minimal Viable Classic Terminal Path

Build the smallest possible thing that is already useful:

- Spawn a PTY
- Parse a solid subset of VT/xterm sequences
- Maintain a correct screen + scrollback
- Render to the terminal (or a simple GPU/software backend)
- Handle basic input

Once this path is solid, everything else can be layered on top without risking classic compatibility.

---

## 6. Repo Structure + README

Initialize the repository with:

- The finalized Project Charter
- Clear vision and non-goals
- Current status / roadmap
- How to build and run the early prototypes
- Contribution / design principles

This becomes the single source of truth as the project grows.

---

### Suggested Order

1. Repo + skeleton + README (with charter)
2. Minimal viable classic terminal path
3. High-level architecture + hybrid rendering model
4. Capability/query protocol
5. Begin multiplexer and rich layer work

These six items give a clean, low-risk on-ramp while protecting the core promise of Prismattyc.
