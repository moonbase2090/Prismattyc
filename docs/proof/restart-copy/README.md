# Restart dialog copy: before and after

The Update and restart dialog in a real app on Linux, from
`tests/native/restart-modal-e2e.py --sessions 3 --keyboard-only` in a Debian
container under Xvfb (built in the container). Both runs PASS; nothing was
installed or restarted.

- `before.png`: main.
- `after.png`: this change. Linux keeps sessions running through a live
  update, so the dialog says so and names the fallback.

On Windows, or with `live_update = false`, the dialog keeps the earlier
wording word for word; the unit test
`restart_modal::tests::a_live_update_says_the_sessions_keep_running_and_names_the_fallback`
covers both.
