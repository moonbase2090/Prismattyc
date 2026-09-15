# Prismattyc documentation

Start with the [README](../README.md) for installation and basic use.

## Use Prismattyc

| Guide | What it covers |
| --- | --- |
| [Configuration](config.md) | Fonts, themes, shortcuts, window appearance, and hot reload. |
| [Sessions and Spaces](spaces.md) | Tabs, panes, saved layouts, and session lifetime. |
| [pmux commands](mux-cli.md) | Session management, scripting, and messaging. |
| [Update and restart](update-and-restart.md) | Install updates, restart components, and roll back. |
| [Troubleshooting](hung-session-recovery.md) | Diagnose an unresponsive pane or session. |
| [Accessibility](accessibility.md) | Keyboard controls and screen-reader support. |
| [macOS](macos.md) | Source builds, application bundles, and platform behavior. |
| [Hyprland](hyprland.md) | Wayland settings and window rules. |
| [SSH sessions](ssh.md) | Connect to a remote session from a terminal. |
| [Terminal compatibility](fidelity-matrix-v1.md) | Supported terminal features and their limits. |

## Integrate with Prismattyc

| Reference | What it covers |
| --- | --- |
| [Pane messaging](pane-write-protocol.md) | Deliver text to a pane and inspect the result. |
| [Rich clients](rich-client.md) | Build applications that use optional rich content. |
| [Capability protocol](capability-protocol.md) | Discover supported application features. |
| [Transport compatibility](capability-transport-matrix.md) | Protocol versions and transport behavior. |
| [Rendering](rendering.md) | Presentation backends and transparency. |
| [Architecture](architecture.md) | Components, process ownership, and data flow. |
| [Terminal input](input.md) | Selection, mouse input, wide text, and keyboard encoding. |
| [Control protocol](control-plane.md) | Local mux socket messages and ownership. |
| [Rich surface protocol](rich-surface.md) | Optional surface geometry, limits, and revisions. |

## Build and test

See [Contributing](../CONTRIBUTING.md) for build and test commands,
[Workspace layout](workspace.md) for the source tree, and
[Release packaging](release-process.md) for the distribution tools.
