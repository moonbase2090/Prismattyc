# Sessions and mail

A session keeps a shell alive while clients come and go. Its agent mailbox retains letters until they are claimed and committed.

## Sub-features

- `session-output`: sending text reaches a real shell and its output is readable.
- `mail-lifecycle`: send, claim, and commit preserve the literal message until acknowledgement.
- `session-identity`: mailbox activity preserves the session and shell process.

## How to get to it (user POV)

Use `pmux ls`, `pmux send PANE TEXT --enter`, and `pmux save-buffer SESSION -`. Use `pmux mail --as AGENT send`, `claim`, and `commit` for mail. Interactive attachment is a separate path through `pmux attach SESSION` and needs a real PTY when changed.

## Driving it with verify-cli.py

Preconditions: build and set the variables in the skill's Launch section; use a new output directory.

- Run `.cursor/skills/verify-prismattyc/scripts/verify-cli.py --bins "$verify_bins" --out "$verify_evidence/sessions-mail" --feature sessions-mail`.
- The helper creates `verify` with session `verifier`, sends `printf 'VERIFY_%s_OK\n' LIVE` to the discovered pane, and requires `VERIFY_LIVE_OK` in `save-buffer` output. The full marker is not present in the typed command.
- It sends body `literal-mail-proof`, claims exactly that letter, commits its returned ID, and requires the next claim to contain no letters.
- Inspect `commands.jsonl`, `pane.txt`, and `result.json`. The final snapshot must preserve the session ID and child PID. Cleanup must preserve those evidence files.

## Gotchas

Pane IDs differ from session IDs. Never send to a guessed ID or use `--force` against another client. The helper is noninteractive and does not prove attach keyboard routing; use the existing PTY integration tests in `crates/prismattyc-mux/tests/interactive_attach.rs` when that path changes. Mail in this fixture stays inside its private daemon.

## Linux test portability

When changing fake foreground-process metadata or the Linux CI runner, run:

```sh
cargo +1.90.0 test -p prismattyc-mux --lib --locked doorbell_ -- --test-threads=1
```

Require nonzero executed tests and inspect the foreground, background-only, and
foreground-versus-background submit-chord cases. These use real processes; a
mailbox-only drive does not exercise foreground process detection.

In `crates/prismattyc-mux/src/control.rs`, `fake_agent_sleep()` selects
`/usr/bin/gnusleep` when present, otherwise `/bin/sleep`. The fixture launches
that executable through a renamed symlink. Ubuntu 26.04's multicall sleep
rejects an unfamiliar executable name, so testing ordinary `sleep` alone does
not verify the fixture. Record which executable exists on the tested OS.

For the Ubuntu 26.04 preview, use that runner's workflow dependency setup
(tmux, real-window display dependencies, and CJK fonts), then run its workspace
test and GPU adapter enumeration commands from `.github/workflows/ci.yml`.
Record the runner OS and each job result. A macOS fallback pass does not prove
the GNU executable path or Ubuntu preview. If that runner is unavailable,
report Ubuntu coverage pending and retain the local targeted result.
