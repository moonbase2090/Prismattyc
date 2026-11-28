# Server card and the launch splash: proof

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
