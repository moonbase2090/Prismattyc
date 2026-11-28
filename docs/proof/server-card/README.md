# Server card: user-flow proof

`tests/native/server-card-e2e.py` drives a real app against a private pmux
server. A `pmux` wrapper makes `pmux up` fail until the test allows it, so
the app's own start fails first. Frames are the app's presented frames.

| Platform | Where | Result |
| --- | --- | --- |
| macOS 26.6.2 arm64 | fresh Tart VM from the base image, built in the VM at cbe22df8, deleted after | PASS, twice |
| Linux | Debian bookworm container with Xvfb and openbox, built in the container | PASS |
| Windows | batched before release | — |

Checks, in order:

1. Failed start: the window opens (the app does not quit), the card shows "The pmux server did not start" with the reason.
2. Try again: Enter starts the server; the card goes away.
3. Stop: `pmux stop` with the Space `work` open; after the grace, one card replaces the attached pane.
4. Start: Enter starts the server; the card goes away and `work` stays current.
5. Local shell: after another stop, the new-tab key opens a local shell; the card steps aside while the server stays stopped.
