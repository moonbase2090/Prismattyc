# pmux — the mux front door

One binary to remember for the Phase 2B mux. It wraps `pmuxd`,
`pmux-attach`, and the ADR-0008 control plane behind tmux-shaped verbs.

## Glossary

| Noun | Meaning |
|------|---------|
| session | The pmuxd mailbox and agent unit. List with `pmux ls`. |
| tab | A host Window. Sessions in one tab appear as panes. The inner pmuxd Window is what `pmux-attach` shows. |
| pane | A host PaneRuntime, or a pmuxd Pane in `pmux ls`. |
| space | An exclusive owner of sessions, persisted at `spaces/NAME.json`. One session can belong to only one space. |
| spaces rail | The strip of space chips, at the bottom by default. Use `+` to create a fresh space. Live session names appear below the Space name. |
| attach-tabs | The host cache `{stem}.attach-tabs.json` next to the mux socket. Never hand-edit it. |

Commands:

```bash
pmux up                  # start the server detached ($SHELL -l)
pmux up -- /bin/bash -l  # ... with an explicit program
pmux attach              # attach; TTY = interactive, else JSON dump
pmux attach work         # attach to a named session
pmux attach work --json # force the JSON ReadPane dump
pmux attach --read-only  # view only; never take the input lease
pmux attach --all        # open prismattyc-host; sessions as panes in tabs
pmux ls                  # sessions → windows (tabs) → panes (liveness, pids, leases)
pmux whoami              # this pane: session name, opaque id, pane, agent
pmux attention work "needs input" # send OSC 9 to the session pane
pmux attention work                  # default message: needs your attention
pmux new work            # create a named session (TTY: attach)
pmux new work --no-attach  # create only; print the attach hint
pmux doctor              # child pid, lease, VIEWER vs NESTED attach
pmux doctor work         # same, one session
pmux render-status --json # live host render guards, frame age, and pane state
pmux kick work           # SIGTERM nested attach, else viewers; session stays
pmux clients [work] [--json]  # attach clients: pid, viewer/nested, session, pane
pmux detach --other [work]    # SIGTERM every attach but your own; sessions stay
pmux status              # socket liveness + server pid + log path
pmux stop                # ShutdownServer over the socket; TERM/KILL fallback
pmux --session work stop # destroy one named session; server stays up
pmux stop work           # same as --session work stop
pmux session clear [--all] [--keep NAME]
                         # stop sessions except the caller
pmux restart             # restart safe components; defer active PTY owners
pmux restart --plan      # inspect restart impact
pmux versions            # installed and running component versions
pmux update              # verified GitHub release artifacts, starting at 0.2.0
pmux update --source --host       # host only (macOS: also rebuild Prismattyc.app)
pmux update --source --mux        # mux / attach / server / pmux-mcp
pmux completions bash    # emit a completion script (also zsh, fish)
pmux config init [--merge]  # write [mux] keys into config.toml
pmux space create NAME             # one fresh shell pane in a new space
pmux space save [NAME] [SESSION...]  # save owned sessions (live cwd + agent)
pmux space open [NAME] [--add] [--replace] [--no-attach] [--new-window] [--no-run] [--tty]  # restore; switch is default
pmux space attach [NAME] [--session S]  # attach a space session in this TTY
pmux space ls            # list saved spaces
pmux space rm [NAME...] [--all]  # delete space files (`delete` is an alias)
pmux space add NAME [--session S] [--tab TITLE]  # fresh or unassigned session
pmux space move NAME --session S              # transfer session ownership
pmux space move NAME --session-id ID          # select an exact numeric session ID
pmux space move NAME --pane P                 # transfer one live pane
pmux space remove NAME --session S  # release ownership; keep the session alive
pmux space clear [--keep NAME]   # delete every space file except kept names
pmux layout save [SESSION] [--name NAME]  # persist the session tree to a JSON file
pmux layout save space [NAME] [SESSION...]  # alias of `pmux space save`
pmux layout apply NAME [--session TARGET] [--agent]
pmux layout apply space [NAME] [--add] [--replace] [--no-attach] [--new-window] [--no-run] [--tty]  # alias of `pmux space open`
pmux layout apply --all [--replace]       # restore every layouts/*.json file
pmux layout ls           # list saved layouts and spaces
pmux sync on [SESSION]   # fan typed input to every pane in the session
pmux sync off [SESSION]  # stop fanning input
pmux sync status [SESSION]
pmux status-set TEXT     # set this pane's attach chrome status
pmux status-set --clear  # clear it
pmux send PANE TEXT [--enter] [--literal] [--force]  # write keys; no 750 ms lease
pmux save-buffer PANE|SESSION FILE [--history]  # write screen text (`-` = stdout)
pmux pipe-pane PANE|SESSION (FILE | --exec CMD)  # stream Output bytes until Ctrl-C or pane exit
pmux rename-pane PANE|SESSION [TITLE...]  # set a pane title; no TITLE clears
pmux rename-pane --session KEY [TITLE...]  # KEY = name or id; one pane; no pane id
pmux break-pane PANE     # move a pane into its own window
pmux join-pane PANE --to TAB [-h|-v]  # move a pane onto another window
pmux arrange SESSION KIND  # retile: main-vertical|main-horizontal|even-h|even-v|grid
```

## Name a session and its mailbox

A host pane has one session and one agent mailbox. Tabs group panes.
A Space groups tabs. New sessions use one name for the session and
agent ID. Suggested names use the Space name and the first unused
number, such as `work-1`.

1. Create a tab or split a pane.
2. Accept the suggested name or type a name.
3. Select **Create** or press Enter.

The popup creates the session only after you accept. Press Escape to
cancel. Use lowercase letters, numbers, and single hyphens. Names must
have 2 to 64 characters. A name must not start or end with a hyphen.
Prismattyc rejects names already used by a session or forwarding address.

The Space `+` popup first asks for the Space name. It then asks for the
first session name. The default tab strip stays visible with one pane.
`tab_strip = "multi"` shows the strip only with multiple tabs.

Right-click a pane and select **Rename session** to change its name and
agent ID. Right-click a single-pane tab to open the same popup. A tab
with multiple panes has a separate tab label.

From inside an existing session, run:

```bash
pmux session name astra-spaces
pmux whoami
pmux mail inbox
```

Use `pmux session name NAME --session KEY` to select a session by name
or numeric ID. `pmux session rename` is an alias for `session name`.
This also binds a session whose agent is unset. The session ID, pane ID,
and running process stay the same. Saved Space references use the new
name. Queued and claimed letters retain their IDs and state. Previous
mailbox addresses forward to the new address, including after a daemon
restart. Those addresses cannot be assigned to another mailbox.

```bash
pmux space create work --session-name work-1 --no-attach
pmux space add work --name reviewer
pmux session suggest --space work
```

Omit `--session-name` or `--name` to use the suggested name automatically.
After a restart, press Enter in an exited pane to reopen its saved session.
Use `pmux session reopen NAME --space SPACE` to do the same from the CLI.
The session keeps its saved name, mailbox, and Space owner.
`pmux mail alias NAME` creates a shorthand. It does not bind a session.

## Work in isolated spaces

Each space owns its sessions. One space can contain many sessions. A
session can belong to only one space. Select `+` to name a new space and
open one fresh shell pane in the initiating window. The previous space
keeps running. Use Save to update the current space.

```bash
pmux space create work
pmux space add work
pmux space add work --session unassigned-session
pmux space open work --new-window
pmux space move work --session SESSION
pmux space move work --session-id 15
pmux space move work --pane PANE
pmux space move work --pane PANE --to-session DESTINATION_SESSION
```

Use `--session-id ID` when you have a numeric session ID, including in
scripts. This selector never matches a session name. A missing ID fails
even if a session has that number as its name. Select exactly one of
`--session`, `--session-id`, or `--pane`. Use `--to-session` only with
`--pane`. The host uses the exact ID for **Move session to space**.

Add rejects a session owned by another space. Move transfers ownership;
it does not share a session. A session move carries every pane. A pane
move carries only that pane into a destination-owned session. Both keep
the existing PTYs and processes. Moving the last session leaves an empty
space.

Click a chip to switch this window. Opening another space in a second
window does not change the first window. Two windows may view the same
space. `--add` cannot combine different spaces in one view.

Space chips show the names of their live sessions in tab order. Full names remain in
the chip menu when the chip must clip text. Set `space_rail_pane_names =
false` to hide the session names. See [Spaces rail configuration](config.md).

Right-click a pane for separate Move pane and Move session actions. Use
Shift+right-click to send the event to the application. Use Menu or
Shift+F10 on a focused rail chip to open its menu. Press Escape to close it.

Host helpers pass `--view-path PATH` on create, open, and save. This
absolute path selects one window layout. The helper does not write the
global cache or wait for another window's ACK. A new window receives its
own seeded layout through `PMUX_VIEW_PATH`.

Version-2 files contain a stable space ID. `pmuxd` reports each session's
owner in `SessionSnapshot.space_id`. Host writer connections bind to that
owner. After a move, the daemon rejects stale input and resize requests.

Version-1 files migrate only when their session references are
unambiguous. Originals remain in `spaces/legacy-backups`. If several
files reference the same session, open reports the conflict. It does not
silently share or clone the running process. Create a fresh space to
start independent work while you resolve old definitions.

### Rename a Space

Run `pmux space rename OLD NEW`. The command retains the Space identity
and live sessions. It serializes the file change with saves and moves.
An existing destination name is rejected. Other windows follow the stable
Space identity when its display name changes.

## Intentional pane writes

Use `pmux pane-write PANE --text TEXT --json` to send literal text directly
to an agent pane. The default `--submit auto` pastes the text and uses the
foreground agent's submit sequence. `--stdin` reads the body from stdin.
Use `--submit enter` or `--submit none` for explicit terminal input.

The command targets one pane even when sync-input is on. It checks the
current child PID and refuses busy or dirty input without taking a lease.
Receipts distinguish complete and partial queueing. They do not claim that
the recipient accepted the message. Read the [pane-write protocol](pane-write-protocol.md)
for wire examples, error handling, and submission modes.

## Send keys

`pmux send PANE TEXT` writes UTF-8 bytes to a pane through `WritePane`.
It does not hold the controller lease for 750 ms the way
`pmux attach --write` does.

- No controller and a clean input ledger: write with no acquire
  (lease-free).
- A live controller or a dirty input ledger (unsubmitted bytes): exit 1.
  Pass `--force` to take over, write, and release. `--force` does not
  hand the lease back, and taking the lease from a live attach also
  revokes that attach's rich viewer grant. An interactive attach stays
  up: it paints `[held]` and re-acquires on the next key after the force
  write releases.
- The refusal is enforced by the server too: a lease-free `WritePane`
  into a pane whose ledger is dirty fails with `InputDirty`, even when
  the client's snapshot looked clean a moment earlier. If a controller
  appears between the snapshot and the write, `pmux send` re-checks once
  and exits 1 naming `--force` when the pane is still held.
- `--enter` appends CR.
- Default `TEXT` interprets `\n`, `\r`, `\t`, `\e`, and `\\`.
  `--literal` sends the characters as typed.
- Extra words after `PANE` join with a space. `--` lets `TEXT` start
  with `-`.
- Writes longer than 64 KiB are split into 64 KiB chunks.
- Sync-input (if on) fans the same bytes to sibling panes.
- Exit 1 if the pane is missing or its child is dead.

`pmux send` is not `pmux mail send`. Mail is the mailbox. This verb is
keys.

## Save buffer and pipe pane

`pmux save-buffer PANE|SESSION FILE [--history]` writes the pane's
visible screen text (ReadPane) to FILE. FILE `-` writes stdout.
`--history` prepends scrollback by walking `ReadPaneStyled` view
offsets. `pmux pipe-pane PANE|SESSION (FILE | --exec CMD)` opens
`SubscribePane` from the current pane-log sequence and writes Output
event bytes byte-exact to FILE (append) or to CMD's stdin. `--exec`
runs `sh -c CMD`. A non-zero `--exec` exit prints a note to stderr
and does not fail `pipe-pane` (tmux parity). Flush errors still fail
the command. Resize and other events are ignored. The stream stops on
Ctrl-C or when the pane exits. A SESSION name works when the session
owns one pane.

## Break pane and join pane

`pmux break-pane PANE` moves that pane into a new window in the same
session (`CreateWindow` + `MovePane`). If the pane is already the only
pane in its window, the command prints that fact and exits 0.

`pmux join-pane PANE --to TAB [-h|-v]` moves the pane onto window
`TAB` (a window id from `pmux ls` in the same session). `-h` splits
beside the target pane (default). `-v` splits above or below it.
`--help` prints help; `-h` is the axis, not help. Cross-session
`--to` exits 1 and does not move the pane.

The host palette actions `break_pane` and `join_pane` are unbound.
`break_pane` extracts the focused host pane into a new tab.
`join_pane` joins it into the previously active tab (else the next tab).
Both use the existing move-pane path and rewrite the attach-tabs cache.

## Arrange

`pmux arrange SESSION KIND` rebuilds the first window of `SESSION`.
It does not spawn or close panes.

| KIND | Layout |
| --- | --- |
| `even-h` | Even columns. |
| `even-v` | Even rows. |
| `grid` | Two even rows. |
| `main-vertical` | Focused pane on the left. Other panes stacked on the right. |
| `main-horizontal` | Focused pane on top. Other panes in a row below. |

`C-\ a` in `pmux-attach` cycles these kinds in table order
(`even-h`, `even-v`, `grid`, `main-vertical`, `main-horizontal`).
Host palette actions `preset_main_vertical` and `preset_main_horizontal`
retile the active tab the same way. They ship unbound.

## Mailbox verbs

`pmux mail` speaks the Mail* protocol on the mux socket:

```bash
pmux mail send operator-id --summary "hi" [--body "..."]  # omitted --body: piped stdin, else empty
pmux mail claim [--json | --ids]    # fetch your letters (open become held)
pmux mail commit <id>...            # acknowledge held letters (gone for good)
pmux mail release <id>...           # return held letters to open
pmux mail inbox                     # open/held depth
pmux mail watch [--timeout 300]     # exit 0 when mail arrives; exit 1 on timeout (not a mux failure); default 300s
pmux mail who                       # bound agents (presence)
pmux mail alias <name>              # bind a shorthand to your agent
pmux mail broadcast --summary "..." # every bound agent except you
pmux mail status                    # socket, reachability, bound agents (no identity)
```

Identity resolves in order: `--as <agent>`, the agent bound to this live
pane, then `$PMUX_AGENT` outside a known pane. There is no other
default — a CLI that guesses who you are sends mail as the wrong agent.

Watcher recipe: `while pmux mail watch; do pmux mail claim --json; done`.
Exit 1 means the wait timed out (default 300 seconds). The mux is not
broken. Start the loop again if you still want to wait.

The manual doorbell form `pmux mail SESSION [--pane ID]` still parses.
The in-process doorbell makes it unnecessary for mux-native mail.
It injects `PMUX_MAIL` even when the recipient pane is focused (the
occupant is the agent). Two gates defer the write: an unsubmitted
composer line (`dirty_input`, still set after lease release until a
CR or 60 s with no input and no output), and recent typing
(`last_input_at_ms` inside 10 s). Busy PTY output (≥ 8 KiB in 3 s)
also defers. Those injects retry every 3 s until a successful write,
peek, or claim. A letter is injected once. Empty inboxes do not ring
again.

The doorbell rings only an agent CLI. When the pane's foreground process is a
plain shell (no known agent CLI in the terminal foreground; a background job does not count), `InjectMail` answers
`deferred_no_agent`: nothing is typed, the `mail` cell stays lit, and the
nudge retries once an agent starts (PT-94).

## Agent attention

Send an attention signal through the mux control plane. The server stores the
latest message for the pane and emits a `PaneAttention` event. `pmux-attach`
re-emits the event as OSC 9 to its interactive parent terminal.

```bash
pmux attention work "needs input"
pmux attention work
```

The second command uses `needs your attention`. The command uses the additive
`RaiseAttention` control request. It does not acquire a controller lease or
write to the agent PTY.

The host shows a coral tab badge and plays the attention cue by default. It
sends an OS notification when the tab is not selected or the window is not
focused. It limits OS notifications to one per pane every five seconds.

The host accepts these producer forms:

- `OSC 9;MESSAGE` from a pane. The mux validates and relays the message.
- `OSC 777;notify;TITLE;BODY`. It stores `TITLE: BODY`.
- Complete `OSC 99` messages without the `m=1` more-chunk marker.

Producer status:

| Producer | Status |
|---|---|
| `pmux attention` | Verified through `RaiseAttention` and `PaneAttention`. |
| Pane OSC 9 relay | Verified through the live mux drain and interactive attach path. |
| Producer A (external agent integration) | Unverified. Do not claim support for a specific agent until its emitted bytes are captured. |

The host rejects invalid UTF-8, control characters, and messages larger than
512 bytes. BEL remains a bell. It does not create an attention signal.

## Layout verbs

`pmux layout` saves and restores window/pane trees. It does not change
the control-plane protocol.

```bash
pmux layout save [SESSION] [--name NAME]
pmux layout save space [NAME] [SESSION...] [--name NAME]
pmux layout apply NAME [--session TARGET] [--agent]
pmux layout apply space [NAME] [--add] [--replace] [--no-attach] [--new-window] [--no-run]
pmux layout apply --all [--replace]
pmux layout ls
```

### Spaces: save the workspace, open it again

The two commands most people need:

```bash
pmux space create NAME     # fresh space with one shell pane
pmux space save NAME       # save the current space
pmux space open            # restore them and open prismattyc-host on all of them
pmux space attach NAME     # attach the space's active session in this TTY
pmux space ls              # list saved spaces
pmux space rm NAME         # delete spaces/NAME.json (`delete` is an alias)
pmux space rm --all        # list then delete every space file
pmux space add NAME --session S [--tab TITLE]
                           # add an unassigned session as a new tab
pmux space remove NAME --session S
                           # release ownership; keep the session alive
pmux space clear           # delete every space file
pmux space clear --keep NAME
                           # delete every space file except NAME (repeatable)
```

On a cold host launch, the registered host asks **Restore last space?**
if its saved attach-tabs cache contains sessions. Choose **Restore** to
restore the saved tabs and focus. Exited panes remain available; press
Enter in an exited pane to reopen it. Choose **Start fresh** or press
Escape to keep the fresh window without restoring panes. The saved cache
and space files stay intact. The host asks once on its first window.
Later windows and explicit attach targets do not show this prompt.

Restore resolves saved session names and checks Space ownership before it
attaches. It does not reuse a numeric session ID from a previous daemon.
If an older cache has stale IDs, restore uses the saved Space layout.
If the Space was deleted or replaced, restore reports the error and keeps
the fresh window. You can still create a Space with `+`.

Restore does not start saved commands. A stopped session shows **Enter to
reopen**. Enter starts the daemon if needed and recreates only that saved
session. Creating or opening a Space also starts a missing daemon.

The Space name dialog accepts Ctrl+A and Ctrl+V. These keys stay in the
dialog. If the first session name is already in use, the session dialog
shows the error and keeps your entry. Choose another name or cancel.
The current Space remains usable after a rejected name.

`pmux layout save space` and `pmux layout apply space` are aliases of
`save` and `open`. Create independent work with `pmux space create prismattyc-work`.
Reopen it with `pmux space open prismattyc-work`. Delete a named file with `pmux space rm NAME`.
`--all` prints the list then removes every space file. Unknown names exit
nonzero and print the path that was missing. `pmux space clear` deletes
every space file and prints `removed NAME` per file. `--keep NAME`
(repeatable) preserves that file. Clear prints nothing when the store
is empty. There is no prompt. Live sessions stay. `pmux layout rm NAME`
does the same for `layouts/NAME.json`. The attach-tabs cache is not
changed.
`pmux space add NAME` creates another fresh session. Pass `--session S`
to add a live unassigned session. `--tab TITLE` sets the host tab title.
Use `pmux space move NAME --session S` for a session already owned by
another space. Use `--pane P` to transfer one pane.

Saving an existing Space replaces its membership with the sessions you save.
Omitted sessions leave the Space. Their processes and mail remain available.
They do not return when you reopen the Space.

`pmux space remove NAME --session S` releases ownership and keeps the
session running. To do this in the host, right-click the session tab or
pane and select **Remove session from space**. This also closes its views
in the current window. It works for an exited session. It keeps the session
and mailbox. Select **Remove and kill session…** to remove the membership
and terminate the session's processes. Confirm the action with Enter.
This also works for an exited session. The CLI equivalent is
`pmux space remove NAME --session S --kill`. Mail remains available.
Empty spaces are allowed. The pane menu has separate
pane and session move actions. The rail's `+` creates a new space with
one fresh shell; it does not save the current arrangement.

A space also records the host's tab arrangement: which sessions share a
tab as panes, in order, with the tab titles. Arrange the window first
(Ctrl+Shift+Alt+PageUp / PageDown moves the focused pane to the previous
/ next tab; Ctrl+Shift+R renames a tab), then save. `space open` puts
the recreated sessions back into the same tabs. A space saved from a
host that was never arranged opens one tab per session.

A pane whose shell runs `pmux new NAME` or `pmux attach` inside the host
is an attach pane the host did not spawn. Under `PRISMATTYC_HOST=1`, those
commands write the attach-tabs cache and do not exec `pmux-attach`. The
host opens a log-replica pane. If a leftover `bash` → `pmux-attach`
child still appears, the host adopts it within a second and promotes
that pane to a log replica. `PRISMATTYC_ATTACH_PTY=1` keeps the nested
child. Then the mark drops when that attach exits (PT-210, PT-306).

`save` writes JSON under `$XDG_DATA_HOME/prismattyc/layouts`, or
`$HOME/.local/share/prismattyc/layouts` when `XDG_DATA_HOME` is unset.
The file name is `NAME.json`. `NAME` defaults to the session name.
`pmux` rejects an empty name and a name that contains `/` or `..`.

A single-session file stores spawn-time cwd. A space file stores the
live child cwd when `/proc/<pid>/cwd` (or the macOS equivalent) is
readable, else spawn-time cwd. Optional `program` on a leaf is the
pane's root spawn binary. `layout apply` does not re-run it.

A space file also stores optional `command` on each leaf: the pane's
foreground process at save time (the shell's active child, via
procinfo). The field is absent when the foreground is the shell
itself. `space open` may type that command into the restored pane.

`apply` does not attach. `apply NAME` creates the session when `TARGET`
does not exist. It adds the saved windows when `TARGET` exists. `TARGET`
defaults to `NAME`. It does not bind an agent unless you pass `--agent`
(the target session name).

`space save` also copies the host cache's active tab index and focused
session name into the space file. `space open` writes them back so the
opened host selects that tab and pane and unzooms. Missing or unknown
values fall back to the first tab and its first pane.

With no `SESSION` list and a non-empty attach-tabs cache, `space save`
saves the sessions the live window shows (cache tab order). Detached but
still-alive sessions are not saved. stdout includes
`saved N session(s) from the live window`. With no explicit view and no usable cache, Save considers non-default
sessions and then enforces exclusive ownership.
An explicit `SESSION` list is unchanged.

Save rejects sessions owned by another space. It never creates two space
names for the same running session. A saved session omitted by the view
cache receives its own tab when you explicitly select it for Save.

`space save [NAME]` writes the definition under
`$XDG_DATA_HOME/prismattyc/spaces`, or `$HOME/.local/share/prismattyc/spaces`.
The name defaults to `default`. `--name NAME` also works.

Open reuses a live session only when its owner matches the requested
space. Missing version-2 sessions retain their saved optional agent
binding; an unbound shell remains unbound. `--replace` adds saved windows.
Reopening a running session never replays its saved command.

Switch removes foreign-space panes from this view while their processes
keep running. It does not keep a foreign caller pane. `--new-window`
opens an independent view. An explicit empty view stays empty when saved;
it never falls back to all daemon sessions.

For newly restored sessions, `space open` may run saved leaf
`command` values under the configured launch policy. It prints the list, then writes `<command>` plus
Enter (CR) to the pane (`WritePane`; lease acquired then released) and
prints `ran <command> in <session>` per seat. Commands come only from
files under `spaces/`. A pane that already has a foreground process is
not re-run. `[mux] space_open_runs_commands` selects who runs:
`agents` (default: sessions that stored an agent id), `all`, or `none`.
`--no-run` skips every command. `$PMUX_SPACE_OPEN_RUNS_COMMANDS` overrides
the file. `layout apply NAME` still does not re-run programs or commands.

If a `prismattyc-host` has registered `{stem}.host.pid` beside the
socket, `space open` writes the cache and that host regroups. stdout
starts with `reused host pid `. This reuse still runs when Linux has no
`WAYLAND_DISPLAY` or `DISPLAY`. `--tty` skips reuse and prints the TTY
recipe instead. `--no-attach` still writes the cache and reuses a live
host; it only skips spawning. With no live host, `--no-attach` prints no
host line. With no live host and no `--no-attach`, it opens a detached
host as `pmux attach --all` does (`opened host pid `). `--new-window` always opens another host. It runs before the
no-display TTY recipe (`--tty` cannot combine with `--new-window`). It
does not overwrite a live pid file and does not write the cache while
another host is registered.

Chip and menu opens run in click order within each host window. The next
helper starts after the previous helper exits and its requested cache update
applies. If an open fails, the host reports the error and tries the next
queued request. The current chip changes only when the host applies the layout.
While an open is pending, the host suspends layout and selection writes to
the cache. The save action also waits. A failed apply keeps writes suspended
until a later cache update applies. Open a Space again to retry.
These rules preserve version 1 Space files and the Restore / Start fresh
choice. They do not serialize commands from separate CLI processes.

An open result separates the host view from its sessions. The toast reports
reused sessions and says that their live layouts are retained. The last result
remains in each window's `last_space_open` field in `pmux render-status --json`
after the toast expires. The result includes its per-window sequence, open
mode, and target. `seats[].saved_panes` counts saved mux panes.
`seats[].live.panes` counts observed live mux panes, not host attach panes.
A session can retain three live panes when its saved layout has two.

Session results compare daemon snapshots before and after the helper.
A matching live session ID means reused. A new ID means a new session.
A missing session or a session with no live PTY child is unavailable.
A failed snapshot means unknown. These observations do not prove that a
saved command or agent is ready; `launch` remains `not observed`.
Separate CLI processes can change sessions between these observations.

A failed or timed-out apply does not select the requested Space. A partial
host apply clears the current label. The result keeps the helper error and
any observed session changes. A helper owned by the registered window has
a 30-second deadline. On that deadline, the host stops and reaps the helper
before the next queued request starts. Sessions that it created remain in
`pmuxd`. The legacy unregistered-window open remains synchronous.
A new-window request reports that this window has no view confirmation.

The host polls the cache once a second, regroups (MovePane; attach
missing sessions; never close a live session), raises the window, and
touches `{stem}.host.ack`. A host that does not ack within 2 s still
exits 0 with `host pid N did not reload; cache written`. When no live
host is registered and Linux has no `WAYLAND_DISPLAY` or `DISPLAY`, or
you pass `--tty`, `space open` restores the sessions and prints one
`pmux attach SESSION` line per session in tab order. The space's
`focused_session` is marked `(active)`. When stdin and stdout are a TTY
and `--no-attach` is absent, it then attaches that active session in
this TTY. Off a TTY it prints the recipe only and exits 0. `pmux space attach [NAME] [--session S]`
creates missing sessions like `open`, then attaches in this TTY
(default: `focused_session`, else the first). It exits 1 off a TTY.
`pmux space` with no verb, or an unknown verb, prints usage and exits 2.

`apply --all` applies every `layouts/*.json` file in name order. It
implies `--agent`. It uses the same skip / `--replace` rule as
`apply space`.

Inner TUI state is out of scope. The last tree is not auto-saved.

### Session clear

`pmux session clear` stops every live session except the one the command
runs in (`pmux whoami`). When you are not inside a pane, no session is
exempt. `--all` also stops the caller, last. `--keep NAME` (repeatable)
preserves a session by name. The command prints `stopped NAME (id N)`
for each stopped session. It exits 0 when nothing was stopped. Stopping
uses the same path as `pmux stop NAME`. Mailbox letters stay. Saved
spaces stay. `pmux ls` lists sessions. There is no `pmux session ls`.
`pmux space clear` does not stop live sessions.

### Clients and detach-other

`pmux clients` lists every live `pmux-attach` on the socket: pid, `viewer`
(host-side) or `nested` (running inside a pane), the session and pane it
targets, and `(you)` on the attach this shell runs under. `pmux clients
SESSION` narrows to one session; `--json` prints the same rows as JSON.
`pmux detach --other [SESSION]` (alias `-a`) sends TERM to every listed
attach except your own and the registered host window's panes (marked
`(host)`; they live under `{stem}.host.pid`), then prints what it
signalled and what it kept. Host panes attached through the pane log
(the default) are not `pmux-attach` processes and never appear here.
Sessions stay; this is `tmux detach -a`. `pmux kick` remains the
targeted form (nested first, else viewers).

### Where things live

The attach-tabs cache includes a `session_names` map and the saved
`space_id`. The host uses these fields to resolve a restart safely.
The cache remains compatible with older files that omit these fields.

| Thing | Who writes it | Where |
| --- | --- | --- |
| Session | `pmuxd` (`pmux new`) | In the mux. List with `pmux ls`. |
| Tab arrangement | `prismattyc-host` | `{stem}.attach-tabs.json` next to the mux socket. Host cache. Never hand-edit it. Safe to delete. `pmux space open` overwrites it from the space file. The registered host is the single writer (`{stem}.host.pid`). |
| Space | you, or `pmux space save` | `$XDG_DATA_HOME/prismattyc/spaces/NAME.json` (else `~/.local/share/prismattyc/spaces/NAME.json`). This is the workspace file. It records cwd, agent, and the pane foreground `command`. `space open` reads commands only from this directory. |
| Spaces rail | `prismattyc-host` reads it | A view over the spaces directory, polled once a second. Rename and delete act on `spaces/NAME.json`; `+` runs `pmux space save`. The current chip is the space named in `{stem}.attach-tabs.json` (`space`), which `pmux space open` writes and the host preserves. |
| Pane log | `pmuxd` | `$XDG_DATA_HOME/prismattyc/pane-log-<instance>.json` for a socket in the instance dir (`$XDG_RUNTIME_DIR/prismattyc/pmux.sock` or `pmux-NAME.sock`; `/tmp/prismattyc-<uid>/` fallback). `pmux.sock` uses instance `default`. Snapshot plus tail. The snapshot is `emulator-state-v1` (`serde_json` of `EmulatorStateV1`). A mid-sequence parser or pending graphics upload omits the snapshot and keeps the tail. A snapshot that fails import is dropped; restore replays the tail. Restore matches session name and pane index inside that session. A new session name gets nothing. A corrupt, version-mismatch, or `replay-v0` file starts empty with one reset line. `PMUX_PANE_LOG=off` disables persist. Any other non-empty `PMUX_PANE_LOG` is an explicit path. An explicit `--socket` outside the instance dir does not write the data dir. PTY children do not resurrect. |

The host rewrites tab records when you open, close, or rename a tab, Detach a non-last tab, move a pane between tabs, or close a pane from the host. Tab select and focus mark the cache dirty and flush when the event loop waits. Child-exit of an attach pane keeps a placeholder and does not drop that session from the cache. Child-exit of a local shell still collapses the slot and updates only `active_tab` and `focused_session`. Drag the gap between two panes to change their split (the cursor turns into a resize arrow over it). Drag a tab chip in the strip to reorder. When a tab holds more than one pane, the strip grows a second row; drag a pane handle (1.5 cells wide) from that handle row onto another tab to move that pane, or onto the empty end of the strip to make a new tab. `break_pane` / `join_pane` are palette actions with no default chord (`break_pane` extracts the focused pane to a new tab; `join_pane` joins it to the previous tab). `move_tab_left` / `move_tab_right` are palette actions with no default chord. `swap_pane_prev` / `swap_pane_next` exchange the focused pane with its neighbour in layout order (splits and ratios stay; focus follows the pane) and `rotate_panes` / `rotate_panes_back` shift every pane one slot, like tmux `swap-pane` and `rotate-window`; all four are unbound by default. CLI verbs for the server come with PT-132. The cache is rewritten after every reorder or move. `space save` copies the cache into the space file.
The host rewrites tab records when you open, close, or rename a tab, Detach a non-last tab, move a pane between tabs, or close a pane from the host. Tab select and focus mark the cache dirty and flush when the event loop waits. Child-exit of an attach pane keeps a placeholder and does not drop that session from the cache. Child-exit of a local shell still collapses the slot and updates only `active_tab` and `focused_session`. Drag the gap between two panes to change their split (the cursor turns into a resize arrow over it). Drag a tab chip in the strip to reorder. When a tab holds more than one pane, the strip grows a second row; drag a pane handle (1.5 cells wide) from that handle row onto another tab to move that pane, or onto the empty end of the strip to make a new tab. `break_pane` / `join_pane` are palette actions with no default chord (`break_pane` extracts the focused pane to a new tab; `join_pane` joins it to the previous tab). `move_tab_left` / `move_tab_right` are palette actions with no default chord. `focus_last_pane` and `last_tab` (no default chord) jump back to the previously focused pane in the tab or the previously active tab, like tmux `last-pane` / `last-window`; the attach-side chord lands with PT-99. The cache is rewritten after every reorder or move. `space save` copies the cache into the space file.

### Host attach panes are log replicas

`prismattyc-host` attaches a mux session by subscribing to that pane's
event log, not by running `pmux attach --session-id ID` as a nested PTY
child. Each attached pane opens two control connections: one parked in
`SubscribePane`, feeding `Output` and `Resize` into the pane's own
emulator, and one that sends keys with `WritePane` under the controller
lease and host geometry with `Resize`. The lease is taken on the first
key and released after 750 ms idle, as `pmux attach` does.

Host-managed seats use the same path. `pmux attach SESSION`, `pmux new
NAME`, and interactive `pmux-attach --session NAME` under
`PRISMATTYC_HOST=1` write `{stem}.attach-tabs.json` so the host opens a
log replica. They do not leave a `bash` → `pmux-attach` child as the
silent seat. The host also matches `pmux-attach --session` argv at pane
spawn, and it promotes a leftover nested attach to a replica.

Consequences you can see:

- The host scrollbar, bottom-right inverse ` N/M ` chip,
  `Ctrl+Shift+Up/Down`, and find work over the attached session's real
  scrollback.
- `pmux ls` and `pmux doctor` report no NESTED attach for host panes.
  Each attached pane shows as two registered clients instead.
- The replica never answers DSR/CPR/DA. Only the pane's PTY owner does.

`PRISMATTYC_ATTACH_PTY=1` restores the old nested-child path. The host
also falls back to it, with a message on stderr, when the subscription
cannot be opened. A nested attach under `PRISMATTYC_HOST=1` then paints
the host scroll chrome: an inverse bottom-right chip and a right-edge
scrollbar, not the left `[scroll N/M]` line. `pmux attach SESSION` in a
terminal outside the host is unchanged.

### Session-ended placeholder

When a host pane that attaches a session loses it, the pane stays.
It shows the session name, the exit reason, and `Enter to reopen`. The
layout does not collapse. A zoomed pane unzooms first. Local shells still
collapse.

Enter respawns `pmux attach --session-id` when `pmux ls` still lists that
session. When the session is gone, the host runs `pmux new --no-attach`
from the last space `space open` passed as `PMUX_SPACE` (else `default`).
It uses the saved agent and the first-leaf cwd. If that space has no row
for the session, the pane prints `session NAME is gone` and stays.

`Ctrl+Shift+W` closes a placeholder like a normal pane: an intermediate
slot is removed; the last placeholder of a non-last tab closes that tab;
the last placeholder of the last tab quits the host.

The mailbox lives in `pmuxd`. After reopen, a letter waiting for that
seat injects `PMUX_MAIL` into the live child.

A nested `pmux-attach` TTY that sees `ChildExited` prints
`session ended: NAME (pane N child exited)`.

## Sync input

`pmux sync` fans typed keys and paste to every pane in a window.
This is the tmux synchronize-panes model. The flag is per-window.
Mouse reports stay per-pane.

```bash
pmux sync on [SESSION]
pmux sync off [SESSION]
pmux sync status [SESSION]
```

In `pmux-attach`, `C-\ s` toggles sync for the attached pane's window.
`pmux-attach` shows `[sync]` as a top-right overlay while the flag is on.

The server writes the same bytes to every pane in the window. Only the
focused pane needs the caller's controller lease. `prismattyc-host`
in-process panes are out of scope.

## Status line

A guest sets short status text for its pane. `pmux-attach` shows it in
attach chrome: on the scroll row while you page history, and as a
top-right overlay in live mode. A bad payload is rejected. It is not
painted.

```bash
pmux status-set TEXT [--pane ID]
pmux status-set --clear [--pane ID]
```

Identity is `$PRISMATTYC_PANE_ID` unless you pass `--pane`. The command
fails with "not inside a pmux pane" when that variable is unset and
`--pane` is omitted.

The text is at most 64 bytes and must not contain control characters.
`pmux status` is the daemon liveness verb. It does not set pane status.

## Pane titles

`pmux rename-pane PANE|SESSION [TITLE...]` sets a pane title (PT-128).
Words join with a space; no `TITLE` clears it. A non-empty rename pins
the title on the server (PT-230). An empty rename clears the pin. While
pinned, a child's OSC 0/2 does not change the host pane title. A plain
terminal attach (`pmux space attach` over SSH) still shows the child's
OSC title; the pin holds in the host.

`PANE` is a pane id; a session name works when the session owns one pane
and is rejected with the pane ids otherwise. `--session KEY` names the
session by name or opaque id (an all-digit positional is always a pane
id). Do not pass a pane id with `--session`. A session with more than
one pane still needs a pane id. Any client may rename; no lease is taken.

- `pmux ls` prints the title after the pane state: `— title "build"`.
- `pmux-attach` shows the title in the status slot of the attach chrome
  when no guest status is set (status wins).
- `space save` stores the title on the pane's leaf (`"title"`).
  `space open` restores it onto new panes, and onto a live skipped
  session only when that pane is unpinned.
- Same limits as status: at most 64 bytes, no control characters.

In the host, a single-pane tab paints the pane title in place of the tab
title while one is set, and hovering a pane handle on a multi-pane tab
shows that pane's title in the title row (the attach relays the title as
its OSC 2 window title; a local shell's own title shows the same way; a
guest status set with `status-set` wins over the title). The palette
action `rename_pane` (unbound) edits the focused pane's title in the tab
chip and, for a live attach pane, runs `pmux rename-pane --session ID
TITLE`. Right-click a pane handle or a tab with one pane to open its
context menu. Select **Rename session** for an attached session or
**Rename title** for a local shell. Right-click a tab with multiple panes
to rename the tab. Press Enter to save the name or Escape to cancel.

## Shared attach

Two `pmux-attach` clients can view the same pane at once. Both start as
observers. The first client that types takes the write lease. The holder
releases it after 750 ms of idle input. Other clients still render.
They do not queue keystrokes. They see a toast
`input held by client <id> — wait or C-\ d` and a `[held]` chip.

`pmux attach --read-only` (and `pmux-attach --read-only`) never takes
the lease. Typed keys are dropped. Scroll and copy still work. The
chrome shows `[ro]`.

The pane geometry follows the most recently active client (tmux
`window-size latest`). A TTY attach fits the pane to its terminal on
attach and on SIGWINCH. A client that reports the same size does not
change the pane. Input in the host makes the host the latest
client again; the host re-fits on its next geometry pass. Set
`[mux] remote_size = "host"` (or `PMUX_REMOTE_SIZE=host`) to keep the
desktop size except when you force a fit.

A non-holder viewer with a smaller terminal still paints a viewport
when the policy is `host`: it clips to its rows and cols, scrolls so
the cursor row stays on screen, and places the cursor on that clipped
cell (never past the last row). The attach chrome shows
`pane 83×57 > 120×40 · C-\ z fit`. A larger terminal anchors the pane
at the top left and fills the rest with chrome background.

`C-\ z` or `pmux attach --fit` resizes the pane to this terminal even
when another viewer is attached. The host reflows on GeometryChanged.

When a TTY client disconnects, the mux restores the last host
geometry so the desktop window is not left small. Detach with
`C-\ d`. The pane child keeps running. Disconnect does not
destroy the session.

## SSH from macOS

`pmux attach SESSION` is a TTY client. It needs a live Unix socket, not
a compositor. That is the SSH path. The attach fits the pane to this
terminal. Detach with `C-\ d` to restore the last host size. The host
shows a `remote WxH` chip while a remote client owns the size.

To restore a saved space over SSH:

```bash
pmux space open NAME --tty
# or, after the sessions already exist:
pmux space attach NAME
pmux space attach NAME --session fable-pc
```

`space open --tty` (and `space open` with no Linux display) prints one
`pmux attach SESSION` line per session in tab order. The active session
ends with ` # active` so the line stays pasteable. In a TTY it then
attaches that session. `pmux space attach` does not write the attach-tabs
cache and does not regroup a live host. Inside `pmux-attach`,
`C-\ n` / `C-\ p` cycle the current space's sessions (`PMUX_SPACE` or
`--space`; without one, every non-default session in `pmux ls` order).
`--space` and `$PMUX_SPACE` are a hint: attach keeps that name only when
`spaces/NAME.json` exists and lists this session. Otherwise it uses the
first space file that lists the session. When no file lists it, those
chords cycle every non-default session.
`C-\ 1`–`C-\ 9` jump by index and are reserved (not forwarded to the
child). Space-attach chrome (`--space` or `$PMUX_SPACE`) shows
`space NAME · session X (i/n)` after the MailAttention letter cell,
with one blank cell between the letter and the text.
When that hinted attach no longer has a space file for the session, the
chrome shows `session X` with no space prefix. A plain attach has no
identity row. Nested attach inside `prismattyc-host` (`PRISMATTYC_HOST=1`)
does not paint that overlay. The space rail and the tab title already
name the space and the session. Mail letter, status chips, and the
viewport hint stay.

On Linux, `pmux attach --all` execs `prismattyc-host`. That needs
`WAYLAND_DISPLAY` or `DISPLAY`. An SSH session from a Mac has neither.
Do not forward Wayland or X11 to open the Linux host.

If `status` or `ls` reports no server over SSH but the desktop mux is
running, the CLI may be looking at `/tmp/prismattyc-<uid>/pmux.sock`.
`status` then names live `pmux*.sock` sockets under `/run/user/<uid>` and
`/run/user/<uid>/prismattyc/`. Point `PMUX_SOCKET` at that path, or export
`XDG_RUNTIME_DIR=/run/user/$(id -u)`.

Full RCA and later windowed-remote plan:
[ssh-mux-attach-spike.md](ssh-mux-attach-spike.md).

## Instances instead of socket paths

`--instance NAME` (or `$PMUX_INSTANCE`, or `[mux] instance` in the
config file) names the socket — `$XDG_RUNTIME_DIR/prismattyc/pmux.sock`
for `default`, else `pmux-<instance>.sock` in that directory. The
pidfile and log sit next to the socket (`.pid` / `.log`). `--socket PATH`
(or `$PMUX_SOCKET`, or `[mux] socket`) remains the expert escape hatch
and overrides the instance. Precedence is CLI, then `PMUX_*`, then the
file. A CLI `--instance` also ignores `$PMUX_SOCKET` and `[mux] socket`, because every pmux pane exports its own server's socket; only `--socket` overrides an instance name.

## Guest discovery environment

Each mux-owned child is stamped at `execve` with:

| Variable | Value |
|---|---|
| `PRISMATTYC_PANE_ID` | Decimal pane id. Never reused for the Domain lifetime. |
| `PMUX_SOCKET` | Absolute path of this server's control socket. |
| `PMUX_AGENT` | Bound agent id of the pane's session. Absent when the session has no `--agent` binding. |
| `PMUX_TUTORIAL_PACK` | In-repo tutorial name (`prismattyc-tutorial-pack`). Always stamped. See [agents.md](agents.md). |

The same tutorial ships in the binary. Run `pmux tutorial`. The embedded
text is canonical.

`pmux tutorial --play` runs the shared walkthrough catalog as text
captions. It uses the same step IDs, expectations, and progress file as
the host (`walkthrough.json` under `$XDG_DATA_HOME/prismattyc` or
`~/.local/share/prismattyc`). `--level ID` starts at that level.
`--reset` deletes the progress file first.

On a TTY the command prints `Level N/M · step i/n — <caption>`, then the
hint. `mux_event` steps wait for a real pmuxd control event. Other steps
complete on Enter (`press Enter when done`). `s` skips. `q` quits. With
no daemon, `mux_event` steps also complete on Enter. The boss step
(`boss.reproduce-space`) polls a daemon Snapshot once a second and
runs `boss_matches`. A match completes the step. Enter prints
`not yet: <diff>`. `s` still skips. With no daemon, Enter completes
with the usual note. Non-TTY stdin prints the current level's captions
and exits 0. The client never mutates mux state to make a step pass.

`PRISMATTYC_SESSION_ID` is not stamped. `MovePane` can change session without
respawn, so a session key would go stale. Inherited or caller-supplied
values of that name are stripped.

`PMUX_AGENT` lets any in-pane tool derive its mux identity from the seat.
`pmux mail` resolves identity in this order: `--as`, the live pane's
session binding, then `$PMUX_AGENT` outside a known pane.
`pmux new NAME` binds `NAME` as the agent by default, so the variable is
present on every agent seat. Pass `--no-agent` to opt out.

The server overwrites any caller-supplied pane and socket values. Split
starts a new child with a new pane id. Move keeps the same process, so
pane id and socket stay correct. A later respawn would re-stamp the same
pane id. Host in-process PTYs do not receive these keys.

`PMUX_SOCKET` here is the same name the CLI uses to choose a socket.
A guest that runs `pmux` talks to the mux that spawned it.

Agents that scrub the environment can fall back to pid-in-tree matching.
This stamp is discovery only.

## Safety properties

- **Live sockets are never replaced or unlinked here.** Stale-leftover
  handling stays in the server's own bind logic
  (`probe_socket_liveness` → Missing/Live/Stale/Foreign). `up` on a live
  socket says "already running"; `up`/`attach`/`stop` all refuse a foreign
  path and leave it intact.
- **Signals only go to verified pids.** `stop` signals a pid only after an
  exact-argv `/proc/<pid>/cmdline` check (argv[0] is `pmuxd` and the socket
  path is its own argument) — substring hits like wrappers or prefix
  paths never match, and the check is repeated before any KILL escalation so
  a pid recycled inside the grace window is never killed. A lost pidfile
  falls back to a `/proc` scan with the same verification plus, when
  readable, proof that the pid holds the bound socket's inode.
- **`up` proves the bind is ours.** Success is reported (and the pidfile
  written) only when the freshly spawned child holds the listening socket's
  inode — a concurrent `up`/`attach` race resolves to "already running"
  instead of overwriting the pidfile with the bind loser's pid.
- The server is started in its own session (`setsid`), so closing the
  launching terminal never HUPs it.

## Relationship to the underlying binaries

`pmuxd` and `pmux-attach` are still usable
directly. `attach` verbs delegate to `pmux-attach` (`--watch`,
`--write`, `--pane`, `--json` pass through). A TTY stdin enters the
interactive client (raw mode, `WritePane`, resize on local winsize
change, detach `C-\ d`). PageUp or `C-\ [` enters copy mode and
scrolls the server-owned history (`[scroll N/M copy]`) when that history is
non-empty.
Empty alt-screen TUIs (Claude Code) stay live so PageUp and the wheel
reach the child. An idle attach re-acquires the controller lease for
that forward, then drops it after 750ms. Esc/q returns to the
live tail.

### Copy mode

Use copy mode to select text from server history. Copy mode is local to
`pmux-attach`. It never forwards keyboard input to the child.

| Key | Action |
| --- | --- |
| Up, Down, Left, Right | Move the copy cursor. |
| PageUp, PageDown, Home, End | Move through history. |
| `/` | Search forward in the visible history window. Type the query; Enter keeps it; Esc cancels the prompt and stays in copy mode. |
| `?` | Search backward in the visible history window. |
| `n` / `N` | Next / previous match. Wrap inside the current viewport. |
| Space or `v` | Start a selection. |
| Space or `v` again | Yank the selection and return to the live tail. |
| `y` or Enter | Yank the selection and return to the live tail. If no selection is active, yank the current line. Enter while a search prompt is open commits the query instead. |
| Esc or `q` | Leave copy mode without yanking. Esc while a search prompt is open cancels the prompt only. |

Yank sends the selected text to the outer terminal with OSC 52. Your terminal
must support OSC 52 for the clipboard update to take effect. Wheel scrolling
uses the ordinary scroll mode and does not start a selection.

Search matches highlight on the painted grid: the current match is inverse;
the others are underlined. The status line shows the query and `i/n` rank.
Search does not pan to history outside the current viewport.

The client disables outer-terminal autowrap so a full-width row keeps
its last column. A non-TTY stdin or `--json` keeps the
JSON `ReadPane` dump. `--styled-json` instead prints the bounded
`ReadPaneStyled` projection, including workspace rows, rich styles, and
semantic clipboard state, for local proof/debug tooling; it does not change
the stable `--json` shape. `attach --all` execs `prismattyc-host` with live mux
sessions grouped into tabbed panes. Each of those panes subscribes to its
pane event log and runs its own emulator; it no longer spawns a nested
`pmux attach --session-id ID` child (see "Host attach panes are log
replicas"). Tab grouping comes from the attach-tabs cache next to the
socket. With no cache, each session gets its own tab. The leftover
`default` session from `up` is skipped when other sessions exist.
On Linux, `attach --all` without `WAYLAND_DISPLAY` or `DISPLAY` does not
exec `prismattyc-host`. It prints the TTY recipe and the resolved socket, then
exits. Use `pmux attach SESSION` over SSH. Do not forward Wayland/X11
to run the Linux host. See [ssh-mux-attach-spike.md](ssh-mux-attach-spike.md).
`Ctrl+Shift+X` detaches the current session tab. The last tab exits the
host. Closing the window also detaches. Server-owned sessions stay. `ls`/`new`/`status`/`stop`/`doctor`/`kick`
speak the control plane directly (`doctor`/`kick` also scan `/proc` for
`pmux-attach`). `ls` prints each window (tab) with its title and
size, then each pane, plus viewer and nested attach pids when present.
`new` then attaches in the invoking terminal by default. TTY attach paints a
viewport toast `session NAME is attached` for 3s (also on the child's
alternate screen). Interactive `pmux-attach` paints from `SubscribePane`
(event-driven). It does not poll `ReadPaneStyled` every 50 ms while the
pane is idle. Keys, leases, and resize stay on the writer connection.
Set `PRISMATTYC_ATTACH_POLL=1` to restore the 50 ms ReadPaneStyled loop
(stderr note). `--json` / `--styled-json --watch` still poll. The chip uses the host focus-border color
(`PRISMATTYC_FOCUS_BORDER` / `focus_border`, default blue). Opt out with
`--no-attach`, `$PMUX_ATTACH_ON_NEW=0` / `$PMUX_ATTACH_ON_NEW=0`, or `[mux] attach_on_new =
false`. A non-TTY `new` still creates and prints the attach hint unless
`--attach` is passed. `doctor` classifies each attach as VIEWER (host-side)
or NESTED (inside the pane child tree). `kick SESSION` SIGTERMs nested
attaches if any, otherwise viewers; it never calls DestroySession and
never signals the pane child. Create/destroy/move/switch window verbs are
on the control plane (ADR-0008); the umbrella CLI still creates
sessions via `new` and lists whatever windows the server has.
`scripts/prismattyc-mux-daemon.sh` is now a thin compat wrapper over this
binary (`start` → `up`).

The wrapper accepts `PMUX_BIN` and `PMUX_SOCKET`. It derives the pidfile
and log from the socket path (`<socket>.pid` and `<socket>.log`). The default
socket is `$XDG_RUNTIME_DIR/prismattyc/pmux.sock`; with `XDG_RUNTIME_DIR`
unset, the binary falls back to `/tmp/prismattyc-<uid>/pmux.sock`.

Tier 2 (`ShutdownServer`) is implemented: `stop` sends the verb
on a Live socket (registered `client_id`, no pane lease) and waits for
the server to flush `ShutdownAccepted`, drop pane runtimes, unlink its
socket, and exit 0. The verified-pid TERM→KILL path remains the fallback
when the socket is not Live, the verb fails or times out, or the pid
survives the grace window. The CLI still never unlinks a socket itself.
Tier 3 (completions + `[mux]` config) is implemented:
`pmux completions <bash|zsh|fish>` prints a script to stdout.
`[mux]` lives in the same file as the host config (`docs/config.md`);
the host accepts the table and ignores it.

## Inspect render guards

Run `pmux render-status` to inspect the registered windowed host.
The command reads a local snapshot. It does not trigger a repaint.
Pass `--json` to print compact JSON for scripts. The default output is
indented for interactive inspection.
Use `--socket PATH` before the command to select another mux.

The host publishes a snapshot at most once per second. The command rejects
snapshots older than five seconds and snapshots from a different host PID.
`status_seq` increases with each published snapshot.
An all-occluded host may wait without publishing. Its snapshot reports
`occluded: true` for each window and returns `snapshot_stale: true` while the
registered host remains alive. A host that predates this command must restart
on a supporting version before the command can return data.

Each window reports `current_panes` at the snapshot time. Each pane includes
its alternate-screen state, scroll offset, stored image count, and rich
layout flags. `last_raster` reports the latest raster attempt. Its timestamp
can be older than the snapshot when the window is idle. Check
`present_succeeded` before treating that attempt as a displayed frame.

`last_raster.guards` lists every active raster guard for that frame.
Published `bell-toasts` follows the live chip list when linger ends
between rasters. The receipt in `last_space_open` outlives that chip.
Some guards force a full repaint. `layout-transition` forces a full repaint
when pane slots change. `chrome-geometry` records a known chip or badge
change; the compositor may repaint only its bounded boxes. `guard_mask` is
the numeric form of that list. The existing
`full_repaint_reason` keeps its priority order for the OSD and render bench.
`raster_mode` is `partial` when the frame retained unaffected pixels and
`full` when the host repainted the window.
Render timing logs also include `full_repaint_guards` as a comma-separated
list. A dash means no guard was active.

The host promotes a frame to full raster when tab-strip text, spaces-rail
chips, scrollbar geometry, or a text selection changes. These surfaces are
outside the bounded damage boxes.

The `osd` guard means that the render timer OSD is visible. OSD output forces
full repaint for the frame.

The `alt-screen` guard means a pane entered or left its alternate screen
since the previous frame. A pane that stays on its alternate screen does
not set this guard. The `scrollback` guard means the scroll offset changed,
or output changed while the pane showed scrollback. Use `current_panes`
to distinguish these events from the pane's current view state.

## Inspect a Space team

Right-click a Space chip and select **Team details**. With keyboard focus
on the rail, press Menu or Shift+F10. Select **Team details** with the arrow
keys. Home and End select the first and last items. Escape closes details.
Use `space_rail_focus` in the command palette to focus the rail.

Details show session names, roles, process state, explicit requests for
input, and separate letter counts. Select a running session and choose
**View session** to focus it. A stopped saved session offers **Reopen session**.
Reopen starts its saved process; it does not recover an agent conversation.
**Last open result** retains the view result and session outcomes after the
toast expires. **Retry opening this Space** reuses live sessions without
repeating saved commands. Desktop Space switches use `--no-run`.

Use these commands for the same details in a terminal:

```bash
pmux space details project --json
pmux space result project --json
pmux space role project builder Reviewer
pmux space link project Repository /absolute/path/to/repository
pmux attention builder "Review the patch"
pmux space attention project --json
pmux space attention snooze project builder PANE REQUEST
pmux space attention resolve project builder PANE REQUEST
```

Copy `PANE` and `REQUEST` from the details response. The daemon checks the
session owner and request revision before changing attention. Snooze lasts
10 minutes. Repeated identical requests coalesce. Resolve, snooze, and
inspection do not claim or commit mail. Typing into a pane clears its
explicit attention request, as it clears the existing attention hint.
When the daemon disconnects, details label retained request reasons as
stale. They do not count as current requests for input.

Roles follow session rename and moves. Context links belong to the Space.
Links may use HTTP(S) or an absolute local path. Opening a link uses your
system handler. Prismattyc does not change external task status.

## Reuse a team template

1. Save the source Space after arranging its sessions and tabs.
2. Open **Team details** and select **Save team template**.
3. Select **Team templates** and choose a saved template.
4. Enter a new Space name.
5. Read the generated session names, directories, programs, commands, and conflicts.
6. Select **Create shells only** or **Create and launch**.

Preview does not create sessions or execute recipes. It also works without
a daemon. Offline previews label the live checks that creation must repeat. Each new team gets
its own sessions, PTYs, ownership, and mailbox names. A repeat of the same
creation request reuses the recorded result. If a launch was interrupted
at an uncertain point, creation reports that uncertainty and refuses to
repeat the command. Inspect the session before starting further work.

```bash
pmux space template save review-team --space project
pmux space template ls
pmux space template preview review-team next-project
pmux space template create review-team next-project --no-attach
# To execute the previewed recipes, explicitly select --launch instead.
pmux space template create review-team another-project --launch --no-attach
```

`pmux-mcp` exposes `pmux_space_details`, `pmux_space_result`,
`pmux_space_role`, `pmux_space_link`, `pmux_space_attention`, and
`pmux_template_save`, `pmux_template_list`, `pmux_template_preview`,
`pmux_template_create`. These tools use the same CLI operations and socket.
Template creation through MCP prepares the team without switching a desktop
view. Its `launch` parameter defaults to false.

## Adjust Spaces preferences

Open the command palette and select `space_settings` in **Spaces**.
You can also right-click a Space chip and select **Spaces settings…**.
Right-click an empty part of the rail to open the same settings.

- Select **Rail: bottom**, **Rail: left**, **Rail: top**, or **Rail: right**.
  The rail moves immediately. The preference applies to all windows.
- Select **Autosave** to turn it on or off. It is off by default.
  When enabled, changed tab arrangements and session layouts save after
  two idle seconds. Saving does not run saved commands.
- Select **Startup: ask**, **Startup: restore**, or **Startup: fresh**.
  The default is **ask**. Restore reconnects running sessions.
  Stopped sessions stay stopped until you explicitly reopen them.

The current Space chip shows its save state. **Unsaved** means the current
arrangement differs from its saved definition. **Save failed** requires a
retry with **Save current space**. Autosave stops retrying after a failure.
Normal terminal output and focus changes do not trigger layout saves.

Crowded chips show a shortened session list and a **+N more** count.
Use **Team details** to inspect the full session list. Select **…** on a
crowded rail to open the searchable Space picker. Keyboard navigation
keeps the focused chip visible on every edge.

## Move a nested session to a Space

The pane context menu moves the terminal that you see. If you run
`pmux new NAME` or a nested `pmux-attach` inside a managed pane, the
move picker shows the nested session name and pane ID. **Move session to
space** moves that session. **Move pane to space** moves its focused pane.
The parent shell stays in its original Space. The nested viewer detaches
after the move. The moved terminal process keeps running.

If the target changes while the picker is open, the move is cancelled.
Open the picker again to select the current terminal.

After upgrading from 0.1.314 or earlier, detach and reattach an existing
nested viewer once. Older viewers do not report their current pane.
The host refuses the move until it can identify the visible terminal.
You do not need to restart `pmuxd`.

## Undo a session removal or move

After **Remove session from space** or a Space move, right-click a Space
chip and select **Undo last removal or move**. You can also run
`undo_space_change` from the command palette.

Undo restores the previous membership without restarting processes.
It applies to the last recorded action in that window. It refuses to
replace newer Space changes or act on a session from a restarted daemon.
**Remove and kill session…** cannot be undone.

For a scripted operation, retain an undo receipt:

```bash
pmux space remove project --session builder --undo-file /absolute/path/remove.json
pmux space undo /absolute/path/remove.json
pmux space move review --session builder --undo-file /absolute/path/move.json
pmux space undo /absolute/path/move.json
```

## Update and restart

See [Update and restart Prismattyc](update-and-restart.md) for release checks,
rollback, coordinated restart, and individual component controls.

## Inspect agent messages

Select **agent_messages** in the command palette. The view lists pending
mail and the latest intentional input queue receipt for each live pane.
Press Enter to focus the exact pane. Navigation rechecks the pane and child
process. It leaves mail unclaimed. Queue receipts never confirm execution.
The latest receipt stays until the pane is removed or the daemon restarts.
