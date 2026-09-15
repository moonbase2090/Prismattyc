# Sessions, panes, and Spaces

A **session** holds running terminal programs. A **tab** displays a window
from a session. You can split a tab into **panes**. A **Space** groups
sessions into a named workspace, such as a project.

Managed sessions run in `pmuxd`. They can remain active when you detach
from a window or terminal client. A Space owns its managed sessions, so
switching Spaces does not mix their session lists.

## Save and open a workspace

Use the desktop Space controls or the command line:

```bash
pmux space save
pmux space open
```

The saved layout includes session and pane arrangement information.
Opening a Space reconnects to its existing managed sessions when available.
Saved layouts cannot preserve running processes across a computer restart.

See [pmux commands](mux-cli.md) for named Spaces, templates, and startup
commands.

## Use blank terminals

A blank terminal belongs to its desktop window. Use a direct blank-tab or
blank-split action when you do not want to create a named managed session.
The command palette lists these actions and their shortcuts.

You can move a blank terminal through its pane context menu without
restarting the shell. Moving preserves its current directory, scrollback,
and shell state.

Blank-terminal restoration is optional and off by default. When enabled,
it saves layout, pane sizes, directories, and focus. A later launch starts
fresh shells. It does not restore the old shell's environment or scrollback.

## Navigate and arrange

- Use the command palette to find session, pane, tab, and Space actions.
- Use the terminal switcher to find a live terminal by name, Space, or directory.
- Drag the inner edge of a side rail to change its width.
- Use **Ctrl+Shift+Z** to expand the focused pane and press it again to restore the layout.

See [configuration](config.md) for creation defaults, restoration settings,
and keyboard shortcuts.
