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
