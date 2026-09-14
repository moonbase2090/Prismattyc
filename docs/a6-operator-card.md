# Prismattyc mux — one-page operator card (§5.6.1 step 2)

Hold **Ctrl+Shift** for the chord strip. No leader. Nested `prismattyc` is
single-pane (classic claim only). This card is for **windowed** `prismattyc-host`.

| Want | Do |
|------|----|
| Three panes | `Ctrl+Shift+\` (or `E`) split right; `Ctrl+Shift+-` (or `D`) split down. Or start `prismattyc-host --panes 3 -- /bin/sh` |
| Focus | `Alt+Arrow` to the nearest pane that way. Focus outline = brand color; unfocused = neutral |
| Marker per pane | Type a unique printable string in each focused pane, Enter |
| Copy | Drag to select; `Ctrl+Shift+C` copies (ADR-0001). Shift+drag if the app owns the mouse |
| Close + reflow | `Ctrl+Shift+W` on a non-last pane. Remaining PTYs stay up |
| Command palette | `Ctrl+Shift+P` opens the host action list. Type to filter. Press `Enter` to run an action or `Esc` to close. |
| Unseen output | Amber `!` on a background pane; OS title shows pane/unseen counts. Focus clears the badge |
| Detach (2B) | `Ctrl+Shift+X` (host footer) leaves this session view. Last tab exits the host. TTY attach: `C-\ d`. Server + children stay. Re-run `pmux attach`. SSH from a Mac: `pmux attach SESSION` (not `--all`; that needs a local display). See [ssh-mux-attach-spike.md](ssh-mux-attach-spike.md) |
| Second session | `pmux new NAME` (attaches in this terminal; `--no-attach` to create only). After closing the host, `pmux attach --all` reopens live sessions as tabbed panes using your saved tab layout (one tab per session until you arrange and rename them; leftover `default` is skipped). Raw `pmux-attach --create-session/--session` still works. |

Focus color: `PRISMATTYC_FOCUS_BORDER=violet` or `Ctrl+Shift+]` / `Ctrl+Shift+[` to cycle forward / back.

Every chord on this card is a default: `[keys]` in `config.toml` rebinds host actions ([ADR-0015](adr/0015-user-keybindings.md); names via `prismattyc-host --help`).

**Not on this card:** remote multi-machine product, Phase 3 rich, inventing A-6 PASS.
**Termwright** covers nested `prismattyc` VT only; it does not drive `prismattyc-host`.
