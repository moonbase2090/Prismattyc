# Use keyboard and accessibility controls

Prismattyc exposes host chrome and the focused terminal through AccessKit.
The OS tree contains tabs, pane names, the current document, selection,
scroll state, and modal choices. It does not create one node per cell.

Open the command palette to find an action and its configured shortcut.
Use arrow keys to move through choices. Press Enter to activate a choice.
Press Escape to close it. These controls include **update_restart**,
**agent_messages**, the terminal switcher, and the Spaces picker.
Accessibility activation uses the same action path and confirmation rules.

Agent Messages exposes the full pane identity and queue status in row names.
Selecting a message focuses its exact live pane. It does not claim mail or
report a command as executed. The visible detail area keeps queue information
readable when a row is too narrow.

Palette secondary text and selection text meet a 4.5:1 contrast ratio against
their nominal background colors. Tests cover every built-in theme. This
check does not certify guest application colors, custom themes, or text
blended with an arbitrary background image. Use opaque chrome and a high
contrast theme when you need predictable contrast.

```toml
[a11y]
os_tree = true
announce = true
```

Both settings default to true. Disable `announce` to stop live-region
announcements while retaining the OS tree. Disable `os_tree` to skip the
native accessibility adapter.

## Verify native accessibility

Tree unit tests verify roles, names, focus, and actions. Native tests must
also inspect the OS bus. A passing tree test alone is not screen-reader
proof. The Linux polish fixture uses a private D-Bus session, accessibility
bus, registry, and display. It retains the native tree separately from PNGs.

```bash
dbus-run-session -- python3 tests/native/polish-e2e.py --bins target/debug \
  --out build/polish/accessibility-check --atspi-only
```

The fixture requires `gdbus`, `dbus-daemon`, `at-spi2-registryd`, Xvfb,
xdotool, and the built application. It never changes the desktop's
accessibility settings. `--atspi-only` fails if the native rows are missing.
A Linux bus inspection does not prove spoken output in Orca or VoiceOver.
Those assistive-technology sessions remain separate platform checks.
