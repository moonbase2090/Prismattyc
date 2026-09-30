# Prismattyc

Prismattyc is a terminal application with built-in session management.
You can run shells and command-line programs in tabs and split panes,
organize them into named workspaces called **Spaces**, and reconnect to
sessions without stopping the programs inside them.

The desktop application is `prismattyc-host`. The `pmux` command manages
sessions from a terminal. A background process, `pmuxd`, keeps those
sessions running when you detach.

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
| Linux x86_64 and ARM64 | Release downloads. Ubuntu 22.04 or newer. Runs on Wayland and X11. |
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
checksums.

## Install on macOS

1. Download the universal macOS disk image from
   [GitHub Releases](https://github.com/moonbase2090/Prismattyc/releases).
2. Open the disk image and move `Prismattyc.app` to `/Applications` or
   `~/Applications`.
3. Open Prismattyc from the Dock.

When the app can create a link in `/usr/local/bin`, it links its bundled
`pmux` there. Otherwise, it uses `~/.local/bin`. Prismattyc does not edit
shell profiles. If `~/.local/bin` is not on your `PATH`, or another `pmux`
comes first, the app prints the path and recovery steps.

To check for an update, run `pmux update --check`. To install one, run
`pmux update`. `prismattyc update` does the same thing. On Linux and
Windows this replaces the installed binaries. On macOS it downloads the
universal app zip, checks `SHA256SUMS-macos`, and replaces
`Prismattyc.app`. When that app's folder is not writable, the update
stops and explains how to run it with admin rights or move the app to
`~/Applications`. The old app is deleted after the new one is in place,
and `pmux update --rollback` is not supported there. Reinstall a specific
macOS version from its DMG on the
[releases page](https://github.com/moonbase2090/Prismattyc/releases).
Quit the app and reopen it from the Dock. Updating does not stop mux
sessions.
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
change shortcuts in the configuration file.

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

The workspace package version is **`0.2.23`**. See the
[v0.2.23 release notes](docs/release-notes-v0.2.23.md) for this version's
changes. A version in source does not necessarily have a published download.
Use [GitHub Releases](https://github.com/moonbase2090/Prismattyc/releases) to
find published builds.

## License

Prismattyc is licensed under the [Mozilla Public License 2.0](LICENSE).
You can use it in commercial products. If you distribute modified MPL-covered
files, you must make their source available under MPL-2.0.

Third-party components retain their own terms. See [license scope and source
availability](NOTICE.txt).
