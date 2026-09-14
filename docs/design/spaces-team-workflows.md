# Complete the Spaces team workflows

Status: Implemented on the Spaces team branch. Validation is recorded in
[the acceptance inventory](spaces-team-validation.md).

This decision updates the [Spaces proposal](spaces-agent-centric-prd.md).
Keep normal navigation quiet. Explain failures and offer a direct recovery
action. Keep the terminal usable throughout each flow.

| Proposal | Decision | Implementation boundary |
| --- | --- | --- |
| 1. Reliable opening and recovery | Implement | Retain open results in accessible details. Expose unavailable sessions and safe retry actions. Keep desktop, CLI, and MCP results consistent. The larger replacement of the file handoff remains proposed. |
| 2. Separate stable seat model | Retire as redundant | Keep the shipped session names, mailbox forwarding, exclusive ownership, and restart resolution. Do not introduce another seat identity or remove existing guarantees. |
| 3. Attention by session | Implement | Show the session and explicit reason. Count sessions separately from letters. Coalesce repeats. Inspect, resolve, and snooze without claiming mail or stealing focus. Recheck the live endpoint before navigation. |
| 4. Team details | Implement | Provide an optional session list with live state, roles, and context links. Label stale or unavailable state. Keep external task state external. |
| 5. Team templates | Implement | Save reusable team definitions. Preview names, directories, commands, and conflicts. Create independent sessions only after an explicit action. Prevent repeated launch on retry. |

## Acceptance scenarios

Use the frozen code revision and private box resources. Run terminal flows
through Termwright. Run window flows through the native X11 fixture. Inspect
the captured images. A source check is not a rendered or runtime pass.

1. Open successfully without a confirmation or focus change.
2. Read the last result after its toast disappears.
3. Inspect an unavailable session and reopen only that session.
4. Retry an interrupted open without duplicate sessions or commands.
5. Report a partial or failed open without naming an unrelated layout.
6. Read equivalent results through the CLI and MCP.
7. Raise the same attention request twice and show one waiting session.
8. Show unread letter counts separately from requests for human input.
9. Inspect, resolve, or snooze attention without changing letter delivery.
10. Move or rename a session and navigate to its current endpoint.
11. Mark disconnected requests stale. Keep their reasons inspectable.
12. Open team details with the keyboard. Escape restores prior focus.
13. Set roles and context links. Preserve them across save and rename.
14. Keep long names and descriptions readable in a narrow window.
15. Save a template without starting or stopping work.
16. Preview a template without creating sessions or executing commands.
17. Reject conflicting or invalid session names before launching work.
18. Create a team with distinct sessions, processes, and ownership.
19. Require explicit consent to execute the previewed launch commands.
20. Retry creation after interruption without launching a command twice.
21. Preserve existing Spaces create, move, switch, save, and restart tests.

Record each scenario as passed, failed, blocked, or not run. Include the
command, exit status, artifact path, and any manual recovery required.
