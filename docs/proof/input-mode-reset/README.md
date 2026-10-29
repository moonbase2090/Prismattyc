# Child input reporting after restore

Restoring a saved screen into a newly spawned process must discard the previous
process's mouse, focus, and bracketed-paste modes. Saved output remains visible.
A live application can enable reporting again through its own output.

## Reproduce

Build all workspace binaries together, then run this fixture on Unix:

```sh
cargo build --workspace --bins --locked
python3 docs/proof/input-mode-reset/verify.py \
  --bins target/debug --out /tmp/input-mode-proof
```

The output directory must be new. The fixture creates a private daemon, isolated
HOME/configuration/data/runtime directories, and a raw input recorder. Its first
child enables application reporting. After the actual
`pmux restart --daemon --stop-sessions --json` command, the replacement child
enables no reporting. Both receive input through a real interactive attach PTY.
The server wrapper explicitly enables workspace restore and configuration disables
start at login. The fixture stops its private daemon and deletes its runtime.

| Assertion | Unchanged 0.3.41 | Fixed source |
| --- | --- | --- |
| Live application receives motion | Yes | Yes |
| Restored child advertises mouse tracking | Yes | No |
| Restored child input | `P` + SGR motion + `NEW` | `PNEW` |
| Old screen text retained | Yes | Yes |

The literal injected report is `ESC [ < 35 ; 5 ; 5 M`. See `baseline.json` for the unchanged result and the pull request Proof section
for the fixed commit's actual received bytes, binary version, and cleanup checks. This fixture tests
attach routing and daemon restoration; it does not prove native mouse handling,
zsh interaction, or desktop composition. Native and clean-VM proof remain pending.

## Other checks

The restore regression covers both saved snapshots and replay-only persistence,
then verifies that replacement-child output can enable reporting again. Exit
regressions run real child processes and check preserved output plus cleared
modes. Additional tests cover replayed exits, partial control sequences, and
attach respawn.

The first local interactive attempt stopped before restart because the proof
runner did not drain the outer PTY while waiting for input. Its frame filled the
PTY buffer. The corrected runner drains that output; the unchanged baseline
then reproduces the behavioral failure. This setup failure is not counted as a
product regression.
