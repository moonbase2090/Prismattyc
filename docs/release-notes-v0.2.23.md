# Prismattyc 0.2.23 release notes

## macOS command-line access

The app links its bundled `pmux` executable into `/usr/local/bin` when it can
create the link there. Otherwise, it uses `~/.local/bin`. Prismattyc does not
edit shell profiles. If `~/.local/bin` is not on `PATH`, or another `pmux`
appears earlier on `PATH`, Prismattyc prints a message with the path and
recovery steps.

`pmux uninstall` removes this link only when the symlink and its ownership
record match the app-created link. It leaves Cargo-installed and other
unmanaged `pmux` files alone. The uninstall guide and CLI reference now
describe this behavior.
