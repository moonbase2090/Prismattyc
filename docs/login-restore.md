# Login and workspace restore

**Start at login and restore workspace** is in Spaces settings. The
`start_at_login` config key controls the same setting. New installations
default to `false`. On the first launch after an upgrade, an existing saved
workspace defaults to `true`. An explicit choice always wins. Registration
records that choice so a new installation stays opt-in after saving a Space.

| Command | Effect |
| --- | --- |
| `pmux login enable` | Enable restore and register this instance at login. |
| `pmux login disable` | Remove its login registration and stop supervision. Live sessions continue. |
| `pmux login status` | Report the setting, registration, and saved workspace. |
| `pmux login sync` | Reconcile registration with config. The host runs this at startup and after a setting change. |
| `pmux doctor` | Include login restore status with daemon diagnostics. |

The usual `--instance NAME` and `--socket PATH` options select the instance.
The preference is shared by instances. Each registered instance has its own
service and checkpoint. After an explicit `pmux stop`, supervision stops
until the next login or service start. `pmux up` can start the daemon again.

## Login services

| Platform | Registration | Restart behavior |
| --- | --- | --- |
| macOS | Per-user LaunchAgent named `dev.prismattyc.pmuxd.<instance-hash>.plist` | RunAtLoad, KeepAlive on failure, 10-second launchd throttle. |
| Linux | A systemd user unit with the same name and a `.service` extension | Enabled for `default.target`, restart on failure after 10 seconds. Requires a user service manager. |
| Windows | Per-user Startup shortcut with the same name and a `.lnk` extension | Starts the supervisor at login. The supervisor restarts a crashed daemon. |

Each registration runs the installed `pmux login run` supervisor without a
host window. The supervisor adopts an existing daemon or starts one. Child
failures use exponential backoff from 1 to 30 seconds. The delay resets after
a minute of uptime. A second supervisor exits without starting children.

The daemon takes an OS file lock before it creates PTYs, then checks the
socket. The lock file stays in place after exit so concurrent starters
cannot lock different files. Only the bound daemon publishes its PID.
Concurrent `pmux up` callers wait for the lock owner to finish startup.

Registration uses the stable installed launcher when an update receipt is
available. `pmux uninstall` unloads and removes these registrations before
removing the executable. `--keep-data` preserves the workspace checkpoint.

## Saved state

While enabled, the daemon checkpoints once per second and at orderly
shutdown. The private, versioned JSON file is under the Prismattyc data
directory's `workspaces/<instance-hash>` directory. Atomic replacement keeps
a partial write from replacing the last complete checkpoint. The key stays
stable when reboot removes the runtime directory or its socket.

The checkpoint contains session, window, and pane IDs; the next unused IDs;
names and titles; split layouts and sizes; launch specifications and working
directories; agent names; Space ownership; and the last registered host's
tabs and focus. Headless mailbox sessions are included. Mail remains in the
existing durable mailbox database. Controller leases and PIDs are never
restored.

The first restore without a checkpoint imports this instance's saved Spaces
and opens the most recently saved Space in the host. Conflicting session
names in those files produce an error. Later restarts use the exact
checkpoint, including an intentionally empty workspace. A malformed
checkpoint produces an error before starting children and remains available
for repair.

Restoration starts new processes from the saved launch specifications.
Foreground jobs in interactive shells follow `mux.space_open_runs_commands`:
agent sessions by default, all sessions with `"all"`, or no foreground replay
with `"none"`. The policy is checked again at restore time. Explicit launch
arguments remain the pane's launch specification. In-memory application
state, shell variables, and terminal scrollback are not process checkpoints.
An abrupt crash can lose changes since the last checkpoint.

An already running older daemon continues undisturbed. It begins writing
checkpoints after it next starts with the new executable. Its saved Spaces
provide the initial migration state.

## Host startup

The host offers **Restore workspace** as its first action. With login restore
enabled, it starts or connects to the daemon in the background and restores
after three seconds. Keyboard navigation cancels the countdown. **Esc**
keeps a fresh window. `space_startup = "fresh"` overrides automatic host
restore. The daemon can restore and run sessions while the host is closed.

Host-owned blank terminals retain the separate
[`restore_blank_terminals` setting](config.md#restore-blank-terminals-after-restart).

## Verification

`cargo test -p prismattyc-mux --test workspace_restart` exercises the real
daemon and CLI in private directories. The cases cover duplicate daemon
starts, concurrent CLI starts, reboot with runtime-directory deletion,
foreground replay and policy changes, crash supervision, intentional stop,
Space migration, explicit opt-out, and malformed checkpoint preservation.

`python3 tests/native/login-restore-e2e.py --bins target/debug --host` also
opens the real host and checks automatic restore through its render status.
The `--case service` mode installs, exercises, and removes a private macOS
LaunchAgent. Other cases do not register a login service.

The neutral Linux demo uses `--case reboot --host --capture FILE.mp4` under
Xvfb. It records the restore prompt and the restored workspace.
