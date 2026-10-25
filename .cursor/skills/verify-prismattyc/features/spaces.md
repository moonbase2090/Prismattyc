# Saved spaces

A saved space groups sessions and remembers a layout. Removing its saved definition keeps the running sessions alive.

## Sub-features

- `space-create-save`: create an isolated workspace and save it.
- `space-open-rename`: reopen the saved space and rename its definition.
- `space-remove`: remove the definition while preserving live shell output.
- `space-reset`: confirm a single/all reset, retain original-byte backups, and release ownership while sessions and mail survive.

## How to get to it (user POV)

Use `pmux space create`, `save`, `open`, `rename`, and `rm`. In the desktop, use the space rail or sidebar menus; their input paths need the separate desktop recipe when changed.

## Driving it with verify-cli.py

Preconditions: the skill's Unix Launch variables and a fresh output directory.

- Run `.cursor/skills/verify-prismattyc/scripts/verify-cli.py --bins "$verify_bins" --out "$verify_evidence/spaces" --feature spaces`.
- The helper creates `verify`, saves it, opens it with `--no-attach --no-run`, and renames it to `renamed`. `space ls` must contain the renamed definition; the actual saved JSON is copied to `saved-spaces/`.
- It runs `space rm renamed`, verifies the saved definition is absent, and requires the original session, shell PID, and literal output to survive.
- Read `commands.jsonl`, `saved-spaces/`, and `result.json` after teardown. A shell launch or saved-command replay is not covered by `--no-run`.

## Gotchas

A session belongs to one space. Use explicit names in automation; avoid `rm --all` or `clear` outside an isolated daemon. CLI removal does not prove a native context menu, confirmation, focus, or next-frame presentation; use [Desktop UI](desktop.md) for those behaviors.

## Reset recovery

Run `cargo +1.90.0 test -p prismattyc-mux --locked --test space_team_cli reset_ -- --test-threads=1`.
The tests start private daemons and temporary data directories. Require three
passing tests. They drive `space reset NAME --confirm NAME` and
`space reset --all --confirm "RESET ALL"`, then inspect backups and live state.
They cover incorrect confirmation, windowless and corrupt definitions, orphaned
ownership, repeated reset, backup failure, retained unrelated ownership, process
identity, and mail preservation. Never substitute the installed daemon or live
saved files. These tests run on Unix; Windows runtime proof remains separate.
