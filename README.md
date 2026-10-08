# Prismattyc

Prismattyc is a terminal application with built-in session management.
You can run shells and command-line programs in tabs and split panes,
organize them into named workspaces called **Spaces**, and reconnect to
sessions without stopping the programs inside them.

The desktop application is `prismattyc-host`. The `pmux` command manages
sessions from a terminal. A background process, `pmuxd`, keeps those
sessions running when you detach.

Graphite is the default chrome as of 0.3.0-rc.1. To keep the previous look,
add this line to `~/.config/prismattyc/config.toml`. It applies live:

```toml
chrome_style = "classic"
```

Rolling back to 0.2.30 keeps that config working. See the
[v0.3.0 release notes](docs/release-notes-v0.3.0.md).

## What you can do

- Open terminal tabs and split them into panes.
- Group sessions into Spaces and save their layouts.
- Detach from a session and reconnect while `pmuxd` keeps running.
- Search terminal history, select text, and copy and paste.
- Change fonts, colors, keyboard shortcuts, and window appearance.
- Use `pmux` to manage sessions from scripts.
- Send messages between local agent sessions with `pmux mail`.

Saved Spaces restore workspace layouts. They do not preserve running
processes across a computer restart.

## Platforms

| Platform | Availability |
| --- | --- |
| Linux x86_64 and ARM64 | Release downloads for systems with glibc 2.28 or newer. Runs on Wayland and X11. |
| macOS, Apple Silicon and Intel | Universal signed and notarized release downloads. |
| Windows x86_64 | Release binaries are available, but Windows is not a supported platform yet. |

## Install on Linux

1. Install the runtime libraries. On Ubuntu:

   ```bash
   sudo apt install libfontconfig1 libxkbcommon0 libxkbcommon-x11-0 libegl1
   ```

2. Download `prismattyc-x86_64-unknown-linux-gnu.tar.gz` from
   [GitHub Releases](https://github.com/moonbase2090/Prismattyc/releases).

3. Extract the archive and run its installer. This example uses x86_64;
   use the ARM64 archive name on ARM64:

   ```bash
   mkdir prismattyc-release
   tar -xzf prismattyc-x86_64-unknown-linux-gnu.tar.gz --strip-components=1 -C prismattyc-release
   ./prismattyc-release/install.sh
   ```

4. Start the application:

   ```bash
   ~/.local/bin/prismattyc-host
   ```

Add `~/.local/bin` to your `PATH` to run `pmux` without its full path.
The archive includes the application, command-line tools, manuals, and
checksums. The installer adds the pmux Agent Skill for detected agents. Pass
`--no-agent-skills`, set `PRISMATTYC_NO_AGENT_SKILLS=1`, or set
`install_agent_skills = false` in the host config to skip that step.

## Install on macOS

1. Download the universal macOS disk image from
   [GitHub Releases](https://github.com/moonbase2090/Prismattyc/releases).
2. Open the disk image and move `Prismattyc.app` to `/Applications` or
   `~/Applications`.
3. Open Prismattyc from the Dock.

On the first launch after installation or an app version change, Prismattyc
installs the pmux Agent Skill in the background for detected agents. Set
`install_agent_skills = false` in the host config or
`PRISMATTYC_NO_AGENT_SKILLS=1` to opt out. Installation errors are written to
`$XDG_DATA_HOME/prismattyc/agent-skills-install.log` (or
`~/.local/share/prismattyc/agent-skills-install.log` when `XDG_DATA_HOME` is
unset); retry with `pmux skills install --agent detected`.

When the app can create a link in `/usr/local/bin`, it links its bundled
`pmux` there. Otherwise, it uses `~/.local/bin`. Prismattyc does not edit
shell profiles. If `~/.local/bin` is not on your `PATH`, or another `pmux`
comes first, the app prints the path and recovery steps.

To check for an update, run `pmux update --check`. To install one, run
`pmux update`. `prismattyc update` does the same thing. The macOS app also
has **Check for Updates…** and **Roll Back Last Update…** in its app menu.
It verifies the published checksum, Developer ID team, and Gatekeeper
notarization before replacing the app. The previous verified app is kept for
`pmux update --rollback`. The app restarts its host and reconnects saved pmux
views. A daemon with active sessions stays running until a safe restart is
possible. Automatic checks are enabled by default. Turn them off in the app menu
or set `automatic_update_checks = false` in `config.toml`.
See [Update and restart](docs/update-and-restart.md).

To preview removal, run `pmux uninstall --dry-run`. Run `pmux uninstall` to
remove Prismattyc. Add `--keep-data` to keep Spaces and configuration. On
macOS, uninstall removes `pmux` only when its link matches the one created by
Prismattyc. See [uninstall details](docs/update-and-restart.md#uninstall).

## Use sessions from the command line

```bash
pmux up                   # Start the session server.
pmux new work --no-attach  # Create a session named work.
pmux attach work          # Connect to it in this terminal.
```

To detach, press **Ctrl+\\**, release the keys, then press **d**.
The session keeps running. Use `pmux attach work` to reconnect.

```bash
pmux ls               # List sessions and panes.
pmux status           # Show the server status.
pmux space save       # Save the current Space layout.
pmux space open       # Open the saved Space in a desktop window.
pmux skills install --agent detected  # Install the pmux Agent Skill for detected agents.
```

See the [pmux command reference](docs/mux-cli.md) for tabs, panes, Spaces,
mail, and scripting commands. The installed command manual is also
available with `man pmux`.

## Configure the application

The desktop application reads `~/.config/prismattyc/config.toml`.
Set `PRISMATTYC_CONFIG` to use a different file. Most settings apply
while the application is running.

```toml
font_px = 16.0
font_ligatures = true
```

Read the [configuration reference](docs/config.md) for all settings.
Platform notes cover [Hyprland](docs/hyprland.md) and [macOS](docs/macos.md).

Useful desktop shortcuts:

| Action | Shortcut |
| --- | --- |
| Open the command palette | Ctrl+Shift+P |
| Search terminal history | Ctrl+Shift+F |
| Copy selected text | Ctrl+Shift+C |
| Paste | Ctrl+Shift+V |
| Expand the current pane, or restore its size | Ctrl+Shift+Z |

The command palette lists available actions and their shortcuts. You can
change shortcuts in the configuration file. Click a command or category tab,
or use the arrow keys and mouse wheel or trackpad to move through the list. The
palette keeps a fixed, window-capped height while its rows scroll; the query,
tabs, selected-command details, and footer stay in place.

## Build from source

You need Rust 1.90 or newer. From the repository root:

```bash
cargo build --workspace --release --locked
./target/release/prismattyc-host
```

The executables are in `target/release/`:

| Executable | Purpose |
| --- | --- |
| `prismattyc-host` | Desktop terminal application. |
| `pmux` | Command-line session management. |
| `pmuxd` | Background session server. |
| `pmux-attach` | Connect to a session from an existing terminal. |
| `pmux-mcp` | Expose session and messaging tools to MCP clients. |
| `prismattyc` | Run the terminal renderer inside another terminal. |

See the [documentation index](docs/README.md) for configuration,
troubleshooting, compatibility, and application integration.

## Version

The workspace package version is **`0.3.27`**. This patch bump is not a
published release. See the [v0.3.0 release notes](docs/release-notes-v0.3.0.md)
for the latest published changes. Use [GitHub Releases](https://github.com/moonbase2090/Prismattyc/releases)
to find published builds.

## License

Prismattyc is licensed under the [Mozilla Public License 2.0](LICENSE).
You can use it in commercial products. If you distribute modified MPL-covered
files, you must make their source available under MPL-2.0.

Third-party components retain their own terms. See [license scope and source
availability](NOTICE.txt).
