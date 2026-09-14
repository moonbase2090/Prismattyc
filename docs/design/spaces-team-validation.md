# Verify the Spaces team workflows

Package: 0.1.308. Base: `45ff12add0cad6b1dfe13d7725981069d90c0031`.
Branch: `feat/spaces-team-workflows`.

This inventory covers Spaces only. The four source documents overlap.
Their rows are acceptance criteria, not independent test executions.
The six required classic Termwright scenarios are regression checks.
They do not count as six additional Spaces scenarios.

The workspace run passed 2,065 tests. Check, Clippy with warnings denied,
formatting, and all six required Termwright scenarios passed. The final
native team box passed 25 checks. The restart box passed eight scenarios.
The race box passed serialized A/B/C navigation, helper failure, apply
timeout, live-layout reuse, and an unavailable session after apply.

The template-layout guard is covered by the final team box, the additional
backend receipt, and the workspace suite. It rejects a changed pane layout before any
launch write. It does not change desktop rendering. Offline previews also work without
starting a daemon or creating a Space. Creation repeats the live checks.

## Read the evidence

| Key | Fixture and evidence |
| --- | --- |
| Team | `demo/docker/spaces-team.sh`; `build/spaces-team-box/team-release/` |
| Restart | `SPACES_TEAM_CASE=restart demo/docker/spaces-team.sh`; `build/spaces-team-box/restart-01/` |
| Race | `SPACES_TEAM_CASE=race demo/docker/spaces-team.sh`; `build/spaces-team-box/race-final/` |
| Window | `space_open_window_tests::isolated_space_windows_create_move_and_render`; workspace test log |
| Backend | `demo/spaces-team-e2e.py --termwright`; `build/spaces-acceptance/offline-complete/` |
| Mux | `cargo test -p prismattyc-mux --locked -- --test-threads=1`; `build/spaces-acceptance/mux-shape-final.log` |
| Workspace | `cargo test --workspace --locked -- --test-threads=1`; `build/spaces-acceptance/workspace-complete.log` |
| Termwright | `scripts/termwright-e2e.sh`; `e2e/artifacts/20260911-103913/` |

Each box receipt records binary hashes. The box uses private sockets,
homes, and displays. Native captures show presented frames and X11 output.
Termwright captures the CLI checks and a real TTY Space attach. Review the
PNGs as well as the assertions.

## Check the original requirements

Source: [Spaces agent-team PRD](spaces-agent-centric-prd.md), S-01–S-16.

| ID | Acceptance criterion | Evidence |
| --- | --- | --- |
| S-01 | Keep separate view, session, and launch outcomes after the toast | Race applied, failure, timeout, unavailable results; Team last-result menu |
| S-02 | Set the current chip only after the intended layout applies | Race A/B/C serialization and timeout; Window poisoned-view rejection |
| S-03 | Preserve processes, PTYs, and mail through navigation | Window switch round trip; Mux reuse; Team TTY attach |
| S-04 | Reject conflicting saved bindings and agent names | Mux exclusive ownership, readable session rename, mailbox failure |
| S-05 | Retain explicit restore and fresh-start choices | Restart; Window restore tests; workspace restore-prompt tests |
| S-06 | Retain changed live layouts on ordinary reuse | Race saved two-pane/live three-pane case |
| S-07 | Separate view changes from command launch; prevent repeat launch | Team shell-only template, explicit launch, retry, cross-Space navigation; Mux no-run |
| S-08 | Target the requested desktop window | Window two-window create, move, and rename follower |
| S-09 | Preserve legacy and keyboard/TTY operation | Restart older cache; Mux ambiguous migration; Team keyboard and TTY attach |
| S-10 | Show sessions, roles, state, and separate attention/mail counts | Team details and attention assertions; native attention capture |
| S-11 | Recheck endpoints; leave mail unchanged on inspection or triage | Team move/rename/stale request; daemon stale-owner test; native focus |
| S-12 | Coalesce requests and label stale reasons | Team deduplication, snooze, disconnect; restart-distinct revision test |
| S-13 | Enforce exclusive ownership during create, add, move, and restore | Mux exclusive Space tests; Window |
| S-14 | Keep CLI and MCP details and retained results consistent | PASS for accepted scope: real stdio MCP comparisons. Live operation-query protocol remains proposed. |
| S-15 | Preview templates and conflicts before independent creation | Team preview, conflict, launch, shell-only, interrupted-intent tests |
| S-16 | Keep work context as links | Team role/link validation and keyboard correction; native details |

## Check exclusive ownership

Source: [exclusive ownership contract](spaces-exclusive-ownership.md).

| Scenario | Evidence |
| --- | --- |
| Create A and B with distinct sessions, panes, and processes | Window and Team independent-create |
| Add three sessions to A without adding them to B | Window fresh-session phase |
| Reject adding A's session to B | Mux `exclusive_spaces_create_add_move_and_reject_stale_writer` |
| Route separate markers in A and B windows | Window rendered marker assertions |
| Switch A/B/A without replacing processes or replaying commands | Window and Mux owned reuse |
| Open a second window without redirecting the first | Window |
| Move an entire session and preserve processes/mail | Team and Mux move tests |
| Move one pane and preserve source siblings | Window and Mux pane-move tests |
| Move the last session or pane and leave an empty source | Window and Mux empty-Space tests |
| Reject competing or stale ownership writers | Mux exclusive transaction and daemon transfer tests |
| Recover an interrupted move or restart after save | Mux ownership journal, interrupted rename; Restart |
| Reject ambiguous legacy references to `default` | Mux `exclusive_spaces_empty_save_delete_and_legacy_conflict` |

The chip correction adds a separate rendered check. Each live session
appears once, even with empty pane titles. Names follow saved tab order.
Rename, ownership changes, and the last pane's exit update the chips.

## Check restart recovery

Source: [restart recovery contract](space-restart-recovery.md).

| Scenario | Restart receipt key |
| --- | --- |
| Create from a bare desktop with no daemon | `create_without_daemon` |
| Correct a duplicate session name in the same dialog | `duplicate_name_retry_and_isolation` |
| Restart only the window and preserve PTY/input | `window_restart_preserves_pty_and_input` |
| Reject recycled IDs and reopen only the intended session | `daemon_restart_recycled_id_and_single_session_reopen` |
| Create another Space after recovery | `create_after_restart` |
| Restore a cache without stable identity metadata | `legacy_numeric_cache_recovery` |
| Restore offline, then explicitly start a session | `restore_without_daemon_then_reopen` |
| Recover from a deleted Space without blocking creation | `deleted_space_fails_without_blocking_create` |

All 21 accepted scenarios below passed within the Linux X11 scope.

## Check the newly accepted scenarios

Source: [implementation decision](spaces-team-workflows.md).

| Scenario | Evidence |
| --- | --- |
| 1. Open without a confirmation or unrelated focus change | Window and Race |
| 2. Read the result after the toast expires | Race and Team last-result menu |
| 3. Reopen only an unavailable saved session | Restart and Team single-session reopen |
| 4. Retry without duplicate sessions or commands | Team repeated create and interrupted-intent recovery |
| 5. Label partial/failed opens without adopting another layout | Race and Window poisoned-view check |
| 6. Read equivalent CLI/MCP results | Team real MCP retained-result and details comparisons |
| 7. Coalesce repeated attention | Team attention-dedup |
| 8. Separate waiting sessions from unread letters | Team attention-dedup-and-separate-mail |
| 9. Inspect, snooze, and resolve without consuming mail | Team backend and native triage |
| 10. Follow moved/renamed sessions at their current endpoint | Team move/rename, stale-owner check, cross-Space focus |
| 11. Label retained offline reasons stale | Team disconnected-attention |
| 12. Use keyboard details and Escape without moving pane focus | Team native keyboard and focus assertions |
| 13. Preserve roles and links through save and rename | Team role transfer/rename, template metadata; stable Space-ID sidecars |
| 14. Read launch recipes in a narrow window | Team 500-pixel native preview; wrapped program/directory/command rows |
| 15. Save a template without starting/stopping work | Team identity and launch-count assertions |
| 16. Preview without creating or launching | Team CLI, MCP, and desktop previews |
| 17. Reject conflicts before mutation | Team template-conflicts |
| 18. Create distinct sessions, processes, and ownership | Team explicit-launch and shell-only creation |
| 19. Require an explicit launch choice | Team shell-only default and launch counter |
| 20. Recover interrupted creation without replay | Team journal-before-commit recovery and uncertain-launch refusal |
| 21. Preserve create, move, switch, save, and restart behavior | Window, Mux, Restart, and Race |

## Bound the conclusions

These runs exercise Linux X11 and Unix sockets. They do not establish
macOS, Wayland, or Windows visual parity. No release installation or live
daemon restart is part of this change. Full mutation and Local Actions
merge gates are separate from this acceptance pass.

Offline reasons are the last details retained on disk. They are labeled
stale, and they do not assert current agent work status. Open results retain
launch uncertainty where a live PTY cannot prove agent readiness.

The first race wrapper copied its successful receipt one directory too
high. The native race assertions and `result.json` passed. The wrapper
failed its final receipt lookup. The corrected wrapper copies the nested
receipt directly. Earlier failed runs remain under `build/spaces-acceptance/`
for diagnosis; they are not included as passing executions.

## Inspect representative captures

- [Session names without pane titles](spaces-team-evidence/session-names.png)
- [Explicit attention and separate mail](spaces-team-evidence/attention-details.png)
- [Readable template launch preview](spaces-team-evidence/template-preview.png)
- [Narrow-window preview](spaces-team-evidence/narrow-preview.png)

The [receipt summary](spaces-team-evidence/receipts.json) records the exact
binary hashes and checks behind these captures. The source documents above
map every accepted scenario to its fixture. A green runtime assertion is
not a claim that merge-only mutation or platform gates ran.
