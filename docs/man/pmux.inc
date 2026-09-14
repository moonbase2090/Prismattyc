[DESCRIPTION]
Prismattyc is a terminal emulator and multiplexer. pmux is its command-line
interface. The daemon owns live sessions, windows, panes, and terminal input.
[INTENTIONAL PANE WRITES]
Use pmux pane-write PANE --text TEXT --json for direct agent collaboration.
Use --submit enter to execute an authorized command in a shell pane.
Use --submit none to leave text unsubmitted.
The command targets one pane and reports complete or partial queueing.
See pmux-pane-write(1) for the full contract.
[UPDATE AND RESTART]
pmux update installs verified immutable release assets from
Moonbase2090/Prismattyc, starting at 0.2.0. --check checks availability.
--rollback restores the previous complete installation.
--source explicitly selects a development source build.
.PP
pmux restart coordinates safe component restarts. --host, --mcp, and
--daemon select components. --plan reports the impact without restarting.
The daemon is deferred while sessions exist. --stop-sessions explicitly
permits terminating every session. Host restart defers while blank
terminals are open. pmux versions reports installed and running versions.
[EXAMPLES]
pmux pane-write 15 --text 'seq 1 10000' --submit enter --json
.br
pmux save-buffer 15 -
.br
pmux space remove Work --session worker --kill
.PP
The last command removes a session from its Space and terminates its panes.
Use it only for the session you intend to stop.
[SEE ALSO]
pmux-pane-write(1), pmuxd(1), pmux-attach(1), pmux-mcp(1), prismattyc-host(1)
