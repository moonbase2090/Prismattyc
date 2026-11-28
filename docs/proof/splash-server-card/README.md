# Server card and the launch splash: proof

## macOS, clean VM

`tests/native/splash-server-card-e2e.py` drives a real app in a fresh macOS
26.6.2 arm64 VM (built in the VM at a9cc6b92, deleted after). The splash is on,
and a `pmux` wrapper fails every server start and counts the attempts. Keys go
to the app's process.

1. With the start failed and the splash up, the card stays hidden for longer
   than any card grace (`macos-after-splash-up.png`).
2. Enter goes to the splash: it starts the session and makes no new server
   start attempt. The card appears once the splash is gone
   (`macos-after-dismiss.png`).
3. With the splash gone, Enter reaches the card: Try again makes a new start
   attempt.

Result: PASS, twice. The same script against main (e9cacb1f) fails at step 1,
"the card must not show while the splash is up". `macos-before-splash-up.png`
is main's frame at that moment: the card covers the splash.

## Linux, container

A real app on Linux (Debian bookworm container, Xvfb and openbox, built in
the container), launched with the splash on and a `pmux` wrapper that makes
the server start fail. Frames are the app's own presented frames.

| Frame | Build | What it shows |
| --- | --- | --- |
| `before-splash-up.png` | main at e9cacb1f | The failed-start card paints over the splash, which still owns the keys: Enter would start a session, not Try again. render-status reported `server_card: true` with the splash up. |
| `after-splash-up.png` | this change at 2d4bb094 | The splash shows; render-status reports `server_card: false`. |
| `after-dismiss.png` | this change at 2d4bb094 | After Escape dismisses the splash, the card shows with the reason. |

The window test `restore_prompt::tests::real_window_choices` checks the
same thing, and fails when the splash check is removed.
