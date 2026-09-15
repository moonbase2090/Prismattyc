# Connect to a session over SSH

Use SSH to attach to a session running on another computer. This displays
the remote session inside your current terminal.

1. Install the `pmux` tools on the remote computer.
2. Make sure `pmux` is on the remote user's `PATH`.
3. Start a session on that computer with `pmux up` and `pmux new work --no-attach`.
4. Connect from your local terminal:

   ```bash
   ssh -t user@host 'pmux attach work'
   ```

Replace `user@host` with the remote login and `work` with the session name.

To detach, press **Ctrl+\\**, release the keys, then press **d**.
The remote session keeps running while its `pmuxd` process is running.

This connection uses SSH's terminal transport. It does not forward a native
Prismattyc desktop window or provide a direct network listener for `pmuxd`.
See the [pmux reference](mux-cli.md) for attach options and
[troubleshooting](hung-session-recovery.md) for an unresponsive session.
