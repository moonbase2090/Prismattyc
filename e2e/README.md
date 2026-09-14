# Prismattyc E2E (Termwright)

Playwright-style terminal E2E for the **nested** `prism` binary.

For the display-free Phase 2A multiplexer acceptance matrix, run
`./scripts/test-phase2a.sh` and see
[`docs/phase2a-proof-harness.md`](../docs/phase2a-proof-harness.md). The
Termwright scenarios remain the nested/shared-VT regression surface.

For the Phase 2B server/attach ownership and lifecycle matrix, run
`./scripts/test-phase2b-server.sh`. The full operator detach/reattach dogfood
remains this harness is intentionally display-free.

`./scripts/test-phase2b-detach.sh` is the process-lifetime E2E. It uses
the real server and attach binaries, retains JSON/text evidence under
`e2e/artifacts/phase2b-detach/`, and deliberately does not claim server-crash
recovery.

## Run

```bash
export PATH="$HOME/.cargo/bin:$PATH"
./scripts/termwright-e2e.sh
# or a single scenario:
./scripts/termwright-e2e.sh classic-shell
```

Requires `termwright` 0.2+ (`cargo install termwright --locked --version 0.2.0`).

## Scenarios

| File | What it proves |
|------|----------------|
| `classic-shell.yaml` | Spawn shell, type with spaces, echo, exit |
| `classic-color.yaml` | Truecolor/ANSI text appears on grid |
| `classic-reflow.py` | Narrow and widen the interactive emulator; preserve a logical output line |
| `classic-keys.yaml` | Enter, arrows leave usable shell (no stuck raw) |
| `a6-nested-marker.yaml` | Nested-only §5.6.1 step-3 analog (unique marker). Not a mux-pane proof |
| `rich-attach.yaml` | Nested 0.1 capability grant and cell-rect attachment |
| `pt-176-key-burst.yaml` | A 60+ byte typed command reaches a nested shell without key loss |
| `pt-85-mail-inject-typing.yaml` | Mux attach: partial line must not receive `PMUX_MAIL` (not in the default runner list) |
| `pt-99-space-attach.yaml` | Mux `space attach` then `C-\\ n` chrome switch (not in the default runner list; gated by `interactive_attach`) |
| `pt-130-copy-search.yaml` | Mux attach copy-mode `/` search (not in the default runner list; gated by `pmux-attach` unit tests) |

`pmux send` (PT-126/139/140) has no Termwright scenario on purpose: the
default runner rewrites `command` to the nested `prismattyc` binary, which
has no mux pane to write to. Its proof is `crates/prismattyc-mux/tests/umbrella_cli.rs`
(`send_keys_*`, `write_pane_lease_free_*`) and
`crates/prismattyc-mux/tests/interactive_attach.rs`
(`send_force_over_live_attach_keeps_attach_alive_and_reacquires`, a real
`pmux-attach` on a PTY).

Copy-mode search (PT-130) is the same class: the runner cannot drive
`pmux-attach` copy mode. Proof is `pmux-attach` unit tests
(`copy_mode_search_*`).

The nested `prismattyc` binary grants only the frozen 0.1/0.2 attachment slice; it
is not a protocol 0.3 workspace host.

Generated from templates in this directory; the runner resolves the declared
`prismattyc` command to an absolute path.

## Artifacts

`e2e/artifacts/<run-id>/` — PNG screenshots + screen text/json per step.  
**Agents: open the PNGs.** That is the human-operator view.

## Windowed host

`prismattyc-host` is not driven by Termwright (no PTY host surface). Validate via:

- `cargo test -p prismattyc-host --locked`
- Human dogfood notes
- Shared emulator fidelity covered by nested Termwright above
