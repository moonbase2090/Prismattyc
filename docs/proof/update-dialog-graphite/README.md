# Update and restart dialogs: clean VM proof

Built and run at `89852831` in a fresh macOS 26.6.2 (arm64) Tart VM cloned
from the Cirrus Labs base image, with Rust 1.90.0. The VM was deleted after
the run. Each frame is the app's own presented frame
(`PRISMATTYC_DUMP_PRESENT`). The run used a private `pmuxd` socket, an empty
home and config directory, and a 118×34 window. Keys were sent to the app
process.

| State | How it was reached |
| --- | --- |
| `menu` | Ctrl+Alt+U (bound to `update_restart` for the run) |
| `menu-wrapped-to-last` | Up from the first action. The selection wraps to Restart daemon and skips the update source and the wrapped description. |
| `result-up-to-date` | Check for updates, real check against the release feed (this build reports 0.3.79; the latest release was 0.3.78) |
| `result-update-available` | Check for updates through a `pmux` wrapper that answers `update --check` with the next patch release. Everything else went to the real `pmux`. |
| `result-failed` | Check for updates through the same wrapper, made to exit 1 |
| `result-versions` | Installed and running versions, real |
| `result-restart-daemon` | Restart daemon with a session running, real (restart postponed) |

Recorded from `pmux render-status --json` during the run:

- Arrow keys, in row indices of the menu: Down 5, Down 6, Up 5, End 9,
  Home 1. Rows 0, 2 and 3 (the update source and the wrapped description)
  were never selected.
- A pointer sweep down the update-available result, 6 points per step,
  selected only rows 4 and 5, the two actions. The status, release and
  explanation rows never took the selection.
- Escape closed the panel.

Synthetic clicks did not reach the app in this VM, so clicks are covered by
the unit tests on the palette hit map, not by this run.
