# Workspace and repository layout

Planned layout for the Rust monorepo. **Not yet created** — this is the target
shape from the starting points and charter technical tenets.

## Planned crates

| Crate | Role |
|-------|------|
| `prism` | Nested classic host binary (TTY-backed; claim harness) |
| `prismattyc-host` | Windowed OS host binary (Phase 1.5; own window) |
| `prismattyc-core` | Shared types, screen model, common errors |
| `prismattyc-emulator` | VT/xterm parsing, PTY spawn and I/O |
| `prismattyc-mux` | Sessions, windows, panes, layouts, control plane, and the Phase 2B server runtime (`pmux` / `pmuxd` / `pmux-attach`) |
| `prismattyc-render` | Hybrid cell-grid + optional rich-layer rendering |
| `prismattyc-protocol` | Capability queries; rich markup / canvas protocol definitions |
| `prismattyc-labs` | wasm32 browser labs: in-memory mailbox mirror of the pmux mail contract + splash attract. The `/labs` UI lives in the separate `prismattyc-website` repository; this crate only builds the wasm package (`wasm-pack build crates/prismattyc-labs --target web`) |

Root `Cargo.toml` will be a **workspace** member list only (no logic in the
root package unless we later want a thin meta crate).

## Early decisions (open until skeleton lands)

| Topic | Lean / default | Status |
|-------|----------------|--------|
| Language | Rust (charter) | Decided |
| Edition | 2021 or 2024 (whatever MSRV supports) | Open |
| MSRV | Pick a recent stable; document in README | Open |
| License | TBD (siblings often Apache-2.0 / MIT) | Open |
| Feature flags | `gpu`, `wayland`, `x11` (names illustrative) | Open |
| Default backend | Software / host-terminal first; GPU later | Lean |

## Repository layout (target)

```text
Prismattyc/
  Cargo.toml                 # workspace
  README.md
  Prismattyc-Charter.md
  Prismattyc-Starting-Points.md
  LICENSE                    # when chosen
  crates/
    prism/
    prismattyc-host/
    prismattyc-core/
    prismattyc-emulator/
    prismattyc-mux/
    prismattyc-render/
    prismattyc-protocol/
    prismattyc-labs/           # wasm32 only; checked with --target wasm32-unknown-unknown
  docs/                      # this tree
  .github/workflows/         # check, test, clippy
```

Local editor and MCP dirs are gitignored (machine paths, not repo identity). See [agents.md](agents.md).

## CI (when code exists)

Minimum gates:

- `cargo check --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace -- -D warnings` (or project-agreed lint level)

Classic-path correctness tests should be cheap and default-on; GPU/backend
tests may be feature-gated.

## Principles that affect layout

- **Classic path stays lean** — keep emulator/parser independent of rich-layer
  types where possible (`prismattyc-emulator` must not depend on a heavy rich DOM).
- **Protocol is its own crate** — apps and tests can depend on
  `prismattyc-protocol` without pulling the full host binary.
- **Render backends are swappable** — traits or feature-gated modules in
  `prismattyc-render`, not scattered `cfg` across the tree.
