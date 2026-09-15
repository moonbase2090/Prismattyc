# Workspace layout

Prismattyc is a Rust 2021 workspace. The minimum supported Rust version is
1.90. The root `Cargo.toml` lists the workspace members and shared version.

## Crates

| Crate | Purpose |
| --- | --- |
| `prismattyc` | Terminal renderer that runs inside another terminal. |
| `prismattyc-host` | Desktop window, fonts, input, configuration, and presentation. |
| `prismattyc-core` | Screen cells, text styles, scrollback, reflow, and damage tracking. |
| `prismattyc-emulator` | Escape-sequence parsing, terminal state updates, and PTY support. |
| `prismattyc-mux` | Sessions, windows, panes, layouts, messaging, and the `pmux` tools. |
| `prismattyc-render` | Shared rendering of terminal and rich content. |
| `prismattyc-protocol` | Application capability and rich-content protocol types. |
| `prismattyc-rich-client` | Client library for applications that use rich content. |
| `prismattyc-labs` | WebAssembly components for the separate website's interactive examples. |
| `pmux-mcp` | MCP access to session and messaging tools. |

## Other directories

| Directory | Purpose |
| --- | --- |
| `assets/` | Application icons and artwork. |
| `docs/` | User guides, technical references, and test documentation. |
| `scripts/` | Build, installation, packaging, and validation tools. |
| `scripts/release/` | Linux release archive builder and installer. |
| `demo/` | Native-window tests, performance probes, and demonstration tools. |
| `e2e/` | Terminal interaction tests and fixtures. |
| `features/` | Acceptance scenarios. |
| `terminfo/` | Terminal descriptions bundled with the emulator. |
| `.github/workflows/` | Continuous integration and release checks. |

Generated builds and test output are not source files. Keep them under
ignored output directories such as `target/`, `build/`, and `e2e/artifacts/`.

See [Contributing](../CONTRIBUTING.md) for validation commands and
[Architecture](architecture.md) for the component relationships.
