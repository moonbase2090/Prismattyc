# Daily Space and terminal workflows

Status: Implemented for package 0.1.314.

The six changes use the existing Space settings, palette, and pane context
menu. See [configuration and shortcuts](../config.md#new-panes-and-tabs).

| Change | Acceptance criteria |
| --- | --- |
| Stable side rails | Left and right rails use a fixed width. Drag the inner edge to resize. Save the width across launches. Renaming a Space does not resize terminal content. |
| Compact Git labels | Update the focused pane's Git label without pointer input. Keep the branch and dirty marker visible in narrow tabs. Show the full label on hover. |
| Creation defaults | Support ask, automatic session, and blank terminal defaults. Provide direct blank and managed tab and split actions. Do not show naming prompts for direct actions. |
| Move a blank terminal | Move through the pane context menu. Keep the process, scrollback, directory, and shell state. Keep the source view active. |
| Optional blank restoration | Default to off. Save tab and split structure, ratios, directories, and focus. Restore fresh login shells. Preserve managed processes. Restore hidden views when opened. Turning restoration off removes this window's recipes. |
| Terminal switcher | Search live managed sessions and this window's blank terminals by name, Space, or directory. Focus the existing target. Reject stale or moved targets without launching a replacement. |

Blank restoration stores layout recipes beside the Space data, under
`local-views`. Each recipe belongs to a window view path. Space ownership
uses stable IDs. Recipes contain no saved command, shell environment, or
scrollback. A missing directory falls back to the home directory.

## Run acceptance checks

1. Build the candidate binaries.
2. Set `PRISMATTYC_BINS` to their directory.
3. Run the daily native fixture:

   ```bash
   SPACES_TEAM_CASE=daily bash demo/docker/spaces-team.sh
   ```

4. Inspect `result.json` and the PNG files in the reported evidence directory.
5. Run the existing Spaces fixture with `SPACES_TEAM_CASE=team`.
6. Run `scripts/termwright-e2e.sh` and inspect its PNG files.

The daily fixture has 11 checks. It covers both rail positions, all direct
creation shortcuts, process-preserving moves, search, Git labels, restart
restoration, stale targets, and disabling restoration. The host unit tests
also cover fresh process IDs, split ratios, Unicode label widths, and
transfer of terminal content.

These native captures validate Linux X11. Termwright validates the nested
terminal scenarios. It does not drive the native host window.
