# Prismattyc 0.2.25 release notes

## Modified Enter keys reach terminal programs

Ctrl+Enter and other modified Enter keys are now encoded for programs in a
pane, following the active Kitty keyboard protocol or xterm modifyOtherKeys
setting.

## Configurable macOS Command shortcuts

Enable `macos_shortcuts = true` to add common Command shortcuts for tabs,
copy and paste, selection, find, clearing scrollback, and font size. Rebind
or disable any action in `[keys]`. Run `prismattyc-host --list-bindings` to
see the active shortcuts. Cmd+Q and Cmd+N remain available by default.
