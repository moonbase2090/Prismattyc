# Start and stop the pmux server: user-flow proof

`tests/native/server-card-e2e.py` drives a real app against a private pmux
server. These frames are the app's own presented frames. The steps below
come after the server card checks in `docs/proof/server-card/`.

| Platform | Where | Result |
| --- | --- | --- |
| macOS 26.6.2 arm64 | fresh macOS VM from the base image, built in the VM at 29a2b094 with this test from efe4356c, deleted after | PASS, twice |
| Linux | Debian bookworm container with Xvfb and openbox, built in the container (same tree) | PASS |
| Windows | batched before release | — |

Checks, in order:

1. Stop from Update and restart: the first Enter on "Stop the pmux server…"
   only shows "Enter again saves N sessions and stops the server · Esc
   cancels", and the server keeps running (`6-stop-confirm`).
2. The second Enter saves every session and stops the server. A pending
   restart-when-idle marker is cleared.
3. While stopped, the same menu offers "Start the pmux server" in place of
   Stop (`7-start-row`). A window with only attached panes shows the stopped
   card (`3-stopped`).
4. Start restores the saved sessions: the session saved by step 2 is listed
   again (`8-restored`).

`pmux stop --save` (the CLI step behind Stop) is covered by
`crates/prismattyc-mux/tests/stop_save_restore.rs`.
