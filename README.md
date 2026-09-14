# <img src="assets/brand/png/prismattyc-tile-64.png" alt="" width="48" height="48" align="center"> Prismattyc

**Classic terminal. Modern surface.**

A native emulator and multiplexer that keeps compatibility with the
existing terminal world and gives applications a markup, styling,
animation, and canvas layer when they opt in. Agent mail is built into
the mux. Prismattyc supersedes the Prism name.

## Why the name

- **TTY** = **T**ele**TY**pewriter. The name is a fossil from the
  1960s-era teleprinters that Unix was first operated from. The hardware
  is gone; the name stayed as the Unix terminal abstraction: line
  discipline, echo, and job-control signals.
- A **PTY** (pseudo-terminal) is the software form: a master/slave pair.
  The slave side looks like a hardware terminal to the child process.
  The multiplexer holds the master side.
- **VT** = **V**ideo **T**erminal, DEC's CRT family (VT100, 1978). Its
  escape sequences — `ESC [ 2 J` clears the screen, `ESC [ 31 m` makes
  text red — became the de facto standard that every terminal emulator
  still speaks.

The lineage: teleprinter (TTY) → CRT terminal (VT100) → software
emulation of both. The hardware changed twice; the protocol never did.

Prismattyc owns the terminal model end to end. `prismattyc-emulator` pairs the
`vte` crate's escape-sequence parser with our own cell grid, scrollback,
and SGR attribute handling — no `alacritty_terminal`, no curses. `pmux`
allocates one PTY per pane and brokers input and output between you and
the child. Agent mail rides the same path: the doorbell writes a token
into the pane PTY, indistinguishable from keystrokes.

## Quickstart (`pmux`)

```bash
cargo install --path crates/prismattyc-mux --bins --locked
pmux up                      # start pmuxd ($SHELL -l)
pmux new work                # named session; binds agent id `work`
pmux attach work             # TTY attach; detach with C-\ d
pmux space save              # snapshot sessions to spaces/default.json
pmux space open              # restore them and open prismattyc-host
pmux ls
pmux status
```

Socket: `$XDG_RUNTIME_DIR/prismattyc/pmux.sock`. Honor `PMUX_SOCKET`.
Wrapper: `./scripts/prismattyc-mux-daemon.sh start`.

Full command reference: [docs/mux-cli.md](docs/mux-cli.md).
Install command manuals with `scripts/install-man.sh`; then use `man pmux`
and `man pmux-pane-write`. See the [pane messaging demo](demo/README.md#watch-intentional-pane-messaging)
for a recorded command-and-cleanup workflow.

## Components

| Thing | Name |
|-------|------|
| Mux CLI | **`pmux`** |
| Mux server | **`pmuxd`** |
| Attach helper | **`pmux-attach`** |
| Nested classic host | **`prismattyc`** |
| Windowed host | **`prismattyc-host`** |
| Mail CLI | **`pmux mail`** |
| MCP adapter | **`pmux-mcp`** |
| Control socket | `$XDG_RUNTIME_DIR/prismattyc/pmux.sock` |
| Mail database | `$XDG_DATA_HOME/prismattyc/mail.db` |

Cargo crate names match the product (`prismattyc-mux`, `prismattyc-core`, …).

## Docs

| Doc | Description |
|-----|-------------|
| [Prismattyc-Charter.md](Prismattyc-Charter.md) | Vision, principles, goals, non-goals |
| [Prismattyc-Starting-Points.md](Prismattyc-Starting-Points.md) | First development slices |
| [docs/README.md](docs/README.md) | Documentation index |
| [docs/roadmap.md](docs/roadmap.md) | Status and ordered work |
| [docs/architecture.md](docs/architecture.md) | Component map and data flow (draft) |
| [docs/hybrid-rendering.md](docs/hybrid-rendering.md) | Cell grid + rich layer model (draft) |
| [docs/capability-protocol.md](docs/capability-protocol.md) | Opt-in feature discovery (draft) |
| [docs/workspace.md](docs/workspace.md) | Planned Cargo layout |
| [docs/testing-policy.md](docs/testing-policy.md) | Merge gates: seam rules, box e2e, CRAP, mutation |
| [docs/agents.md](docs/agents.md) | How to work in this repo (mux identity, mail, tests) |
| [docs/termwright.md](docs/termwright.md) | E2E TUI testing with Termwright |
| [e2e/README.md](e2e/README.md) | Termwright scenarios (`./scripts/termwright-e2e.sh`) |
| [docs/mux-cli.md](docs/mux-cli.md) | `pmux` command reference |
| [docs/PRD.md](docs/PRD.md) | Product requirements (PRD v0.5) |
| [crates/prismattyc-labs/README.md](crates/prismattyc-labs/README.md) | Browser labs WASM crate: mailbox teaching mirror + splash (UI lives in the separate `prismattyc-website` repo) |
| [Spaces agent-team proposal](docs/design/spaces-agent-centric-prd.md) | PRD, current topology, program flows, and phased roadmap (proposal) |
| [docs/fidelity-matrix-v1.md](docs/fidelity-matrix-v1.md) | Supported classic claim (`prismattyc-classic/0.1.1`; package `0.1.300`) |
| [docs/spike-baseline-v0.md](docs/spike-baseline-v0.md) | Phase 0A internal baseline |
| [docs/phase-0b-spike.md](docs/phase-0b-spike.md) | Experimental rich spike (off by default) |

## Windowed host (`prismattyc-host`)

```bash
cargo install --path crates/prismattyc-host --locked
# FreeDesktop icons + .desktop (menu entry):
./scripts/install-prismattyc-host-desktop.sh
prismattyc-host
```

The windowed host supports opt-in, render-only OpenType ligatures for terminal
text. Set `font_ligatures = true` in the host config; cell widths, PTY sizes,
selection, and hit-testing remain unchanged.

The host runs natively on Wayland (KWin and Hyprland, with real window
transparency via `wl_shm` ARGB8888) and on X11. Hyprland notes and window
rules: [docs/hyprland.md](docs/hyprland.md).

From a Prismattyc checkout, `prismattyc update` (or `pmux update` if `prismattyc` is not on PATH) pulls `main` and reinstalls host, mux bins, `pmux-mcp`, and `prismattyc`. `--host` or `--mux` installs only that package (`--mux` includes `pmux-mcp`). On macOS that also rebuilds `Prismattyc.app` (`~/Applications` and any other existing copy). Quit the running app and reopen it; `~/.cargo/bin/prismattyc-host` is not what the Dock launches.

Brand mark: [assets/brand/](assets/brand/) · brief [docs/brand/logo-brief.md](docs/brand/logo-brief.md)

## Status

Package **`0.2.0`**. Daily use is `prismattyc-host` against a local `pmuxd`.

**Classic claim:** `prismattyc-classic/0.1.1` (matrix F1–F19). Nested
`prismattyc` is the claim harness. Tag **`v0.1.0`** is the Phase 1 baseline.

**Shipped with the mux:** detach/reattach (`pmux` / `pmuxd` / `pmux-attach`),
agent mail (`pmux mail`), session seats (`pmux new NAME` sets `$PMUX_AGENT`),
and `pmux tutorial` (`pmux tutorial --play` for the shared walkthrough).

**Not in the product claim:** experimental rich (`--experimental-rich`).
Phase 3 entry is open. Remote attach is a first slice. A-6 operator PASS is
deferred.

See [docs/fidelity-matrix-v1.md](docs/fidelity-matrix-v1.md),
[docs/roadmap.md](docs/roadmap.md), and [docs/mux-cli.md](docs/mux-cli.md).

### Host selection cheatsheet (classic)

| Action | Input |
|--------|--------|
| Drag select | Left mouse (works in **scrollback view** too). When the app enables mouse (vim/htop), **plain** click goes to the app; **Shift+drag** still host-selects ([ADR-0003](docs/adr/0003-hybrid-mouse.md)) |
| Word / line | Double / triple click |
| Mark + grow | **Shift+arrow** (primary); **Ctrl+2** mark; Ctrl+Space when the outer host delivers it and no active IME consumes it (desktop/IME may consume Ctrl+Space or Ctrl+2) |
| Home / End / page (select mode) | In select mode, or with Shift while selecting |
| Select all viewport | Ctrl+Shift+A |
| Copy | Mouse-up after drag; Ctrl+C with multi-cell selection; Ctrl+Shift+C |
| Clear | Esc (also clears when the child prints) |
| Find in history | **Windowed host:** Ctrl+Shift+F (also `;` `'` `.`). **Nested under Kitty:** Ctrl+Shift+; (Kitty often steals F and /). `/` while scrolled also opens find. Type query (live first match, case-insensitive); prompt shows **`n/m`**. **Enter** / **F3** next, **Shift+Enter** / **Shift+F3** previous, **Esc** exit |
| Command palette | **Ctrl+Shift+P** opens the command bar: type to filter, **Ctrl+←/→** cycles the chips (All · Panes · Tabs · Layout · Spaces · View & Edit), RECENT lists the last five actions run, a detail box names the chords and config key. **Up** / **Down** selects. **Enter** runs; `select_tab_1…9` and `layout_2…9` ask for the digit next. **Esc** closes. The palette consumes keys while it is open. |
| Zoom pane | **Ctrl+Shift+Z** (`zoom_pane`) gives the focused pane the whole tab; press again to restore the split. The split tree does not change; the tab label shows `[Z]`. Splitting, retiling, moving a pane, or focusing a hidden sibling leaves zoom first. |
| Paste image or file | **Ctrl+Shift+V** pastes ordinary text first. A clipboard image or one image file pastes its path; image data is saved under `$XDG_RUNTIME_DIR/prism-paste` and the last 8 PNG files are kept. |

### Scrollback view

| Action | Input |
|--------|--------|
| Pan history | **Mouse wheel** (≈3 rows); **Ctrl+Shift+Up/Down** (1 row); **scrollbar** drag or click-jump |
| Page history | **Shift+PageUp** / **Shift+PageDown**, or **Shift+wheel** |
| Jump oldest / live | **Shift+Home** / **Shift+End** |
| Back to live | Type any key (or Shift+End) |
| Bare PageUp/Home/End/arrows | Still go to the **child** (less/vim/readline) |

While scrolled (defaults **on**; opt out via env). Nested `prismattyc` and
`prismattyc-host` both show this chrome:

| Chrome | Default | Disable |
|--------|---------|---------|
| Bottom-right inverse chip ` N/M ` | on | `PRISMATTYC_SCROLL_CHIP=0` (also `false` / `off`) |
| Window / OSC title `… scroll N/M` | on | `PRISMATTYC_SCROLL_TITLE=0` |

Both show `· new` if the live bottom moved while you were scrolled.

## Agent seats

`pmux new NAME` binds `NAME` as the mailbox address for that session.
Inside a pane, `$PMUX_AGENT` is the seat id. Local editor/MCP dirs
(`.claude/`, `.cursor/`, `.codex/`, `.grok/`, `.kiro/`, `.mcp.json`) are
gitignored. Guide: [docs/agents.md](docs/agents.md).

### Child `TERM` identity

Prismattyc forces the child PTY environment (never inherits Kitty/Ghostty/etc.):

| Variable | Value |
|----------|--------|
| `TERM` | `prismattyc-kitty` by default (24-bit, alias `prismattyc-direct`); `prismattyc-256color` or `prismattyc-16color` when `PRISMATTYC_COLOR` is `256` or `16`. Last system fallback is `xterm-256color` (never a bare `-direct` name). |
| `TERMINFO` | Path to bundled [`terminfo/`](terminfo/) database (override with `PRISMATTYC_TERMINFO`) |
| `TERM_PROGRAM` | `prismattyc` |
| `COLORTERM` | `truecolor` in 24-bit and 256 modes; unset for 16-color |
| `PRISMATTYC_COLOR` | Optional: `truecolor` (default), `256`, or `16` (`ansi`/`8` also pin 16) |

Rebuild compiled entries after editing a source:

```bash
tic -x -o terminfo terminfo/prism-direct.src
tic -x -o terminfo terminfo/prism-256color.src
tic -x -o terminfo terminfo/prism-16color.src
```

`tic -x` writes 32-bit extended files (`terminfo/p/`, magic `0x021E`) that
carry user caps (`fullkbd`, `Tc`, `setrgbf`/`setrgbb`, paste, focus). Spawn
copies those into the letter subdir of the child's `TERMINFO`. macOS system
ncurses 6.0.x cannot read that format, so spawn also copies 16-bit fallbacks
(`terminfo/legacy/p/`, magic `0x011A`) into the hex subdir (`70/`). Homebrew
ncurses on macOS uses `p/` and sees the extras; `/usr/bin/tmux` uses `70/`
and still loads. Do not compile the extras with `/usr/bin/tic` — it drops
them.

## Build

Prismattyc is a Rust 2021 workspace with a minimum supported Rust version (MSRV) of
**1.90**.

```sh
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
# Nested classic host (claim gate; runs inside Kitty/Ghostty):
cargo run -p prismattyc -- /bin/sh
# Windowed host (Phase 1.5 — own OS window, no outer terminal):
cargo run -p prismattyc-host -- /bin/sh
# Long-lived local mux server + thin attach client (Phase 2B):
cargo install --path crates/prismattyc-mux --bins --locked
./scripts/prismattyc-mux-daemon.sh start          # background daemon (socket under $XDG_RUNTIME_DIR)
./scripts/prismattyc-mux-daemon.sh status
./scripts/prismattyc-mux-daemon.sh attach --json --watch
# or:
# cargo run -p prismattyc-mux --bin pmuxd -- /bin/sh
# cargo run -p prismattyc-mux --bin pmux-attach
# optional experimental rich (Phase 0B; off by default):
# cargo run -p prismattyc -- --experimental-rich /bin/sh
# nested-PTY host UX scripts (scroll / find under a real prismattyc binary):
# cargo test -p prismattyc --test nested_pty_ux --locked
# display-free Phase 2B server/attach architecture proofs:
./scripts/test-phase2b-server.sh
# deterministic detach/reattach process-lifetime proof:
./scripts/test-phase2b-detach.sh
```

| Binary | Role |
|--------|------|
| `prismattyc` | Nested classic host (TTY-backed). **`prismattyc-classic/*` claim harness.** |
| `prismattyc-host` | Windowed OS host ([ADR-0006](docs/adr/0006-windowed-host.md)). Daily-driver path. |
| `pmux` | Mux front door (`up` / `attach` / `ls` / `new` / …). |
| `pmuxd` | Long-lived local PTY/emulator and mux owner ([ADR-0011](docs/adr/0011-long-lived-mux-server.md)). |
| `pmux-attach` | Thin same-user reference attach client for snapshot/events/leases/content/input. |

Host UX is covered by unit tests, nested outer-PTY scripts, and human
dogfood — see [docs/testing-ux.md](docs/testing-ux.md).

Workspace crates: `prismattyc`, `prismattyc-host`, `prismattyc-core`, `prismattyc-emulator`,
`prismattyc-mux`, `prismattyc-render`, and `prismattyc-protocol` — see
[docs/workspace.md](docs/workspace.md).

## License

TBD.
