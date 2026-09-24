# Native Windows host and PMUX

The Windows implementation builds the desktop host, classic terminal, PMUX CLI,
daemon, attach client, and MCP server as native x64 executables. Child terminals
use ConPTY. The host uses the existing tile presenter and damage tracking.

## Build and package

Use Windows 10 version 1809 or newer, or Windows 11, with Python 3.11+, Git,
Rustup, and Visual Studio Build Tools with the C++ workload and Windows SDK.
From a checkout of the desired revision:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/release/build-windows.ps1
```

The script builds the six executables with Rust 1.90 and the static C runtime.
It invokes each executable with `--version`, validates the PE architecture,
and writes a ZIP, individual updater assets, source revision metadata,
licenses, and SHA-256 checksums to `build/windows-release`. Use `-Output PATH`
for another new output directory. The default target is `x86_64-pc-windows-msvc`.
The GNU target additionally requires a native MinGW toolchain on PATH.

Extract the ZIP into a directory you own. Launch `bin\prismattyc-host.exe`, or
add its `bin` directory to your user PATH. The default shell is `%COMSPEC%`,
normally `cmd.exe`. Select PowerShell with `-- powershell.exe` or `-- pwsh.exe`.
Windows code signing is a separate release step; Apple signing does not apply.

The manual `Windows package` workflow runs the same script and uploads its
output as a build artifact. It does not publish a GitHub release or install
anything on a user's machine.

## Unsigned cross-compiled preview

A preview is separate from production packaging and is not accepted by the
updater. With Python 3.11+, a clean checkout, and all six GNU Windows binaries
built from that revision, package it without executing Windows programs:

```sh
python3 scripts/release/package-windows-preview.py --repo . \
  --bins target/x86_64-pc-windows-gnu/release --out build/windows-preview
```

Use a new output directory. The ZIP name and manifest contain the full source
revision. It includes all six executables, licenses, `source.tar.gz`, source
metadata, and file checksums; `SHA256SUMS` covers the ZIP. The packager checks PE
architecture and the embedded short revision, which is a consistency check,
not proof of binary provenance. The preview explicitly records unsigned,
cross-compiled, native-runtime-unverified status. Production packaging still
requires Windows and native executable validation.

Review the package origin before running its included installer. Extract the
whole ZIP, then run `powershell -NoProfile -ExecutionPolicy Bypass -File
.\install.ps1` from its directory. It verifies the manifest files and bounded
`--version` probes, installs into a new per-user directory under
`%LOCALAPPDATA%\Programs\Prismattyc\windows-preview-<full-revision>`, and
creates a revision-specific Start menu shortcut. Optional `-AddToPath` prepends
its bin directory to the user PATH; omit it to keep command selection unchanged.
No running executable is overwritten and no host or daemon is started or stopped.
Version probes do not establish interactive runtime or performance correctness;
checksums do not authenticate an unsigned publisher. The preview uses the normal
configuration and runtime locations, so launching it may connect to an existing
daemon. Do not stop that daemon to try the preview while it owns active sessions.

## Platform boundaries

- The local control protocol remains NDJSON over native AF_UNIX sockets.
  Windows socket support and ConPTY establish the Windows 10 1809 minimum.
- Default sockets live under `%LOCALAPPDATA%\Prismattyc\run`. The directory
  belongs to the current user and has an inheritable user-only DACL before a
  socket is bound. Reparse-point directories and foreign-owned endpoints are
  rejected. An explicit socket path needs a parent directory with the same
  private DACL; the daemon does not change permissions on arbitrary parents.
- Configuration uses `%APPDATA%`; persistent data uses `%LOCALAPPDATA%`.
  Explicit XDG configuration/data overrides are still accepted.
- Attach input uses one bounded console reader and native wait handles.
  The existing paint subscription wakes the renderer. Platform adaptation adds
  no decorative passes, additional frame timer, or full-grid redraw policy.
- Windows has no Unix foreground process group. Foreground discovery uses one
  process-tree snapshot and a batch command refresh, capped at 256 processes in
  a pane tree. It follows a single shell-child chain and verified PMUX
  forwarding children to the running command. Waiting forwarding parents are
  excluded from nested-viewer selection. Branching forwarding chains and
  unreadable metadata remain unknown.
  A shell without children is also unknown: process metadata cannot distinguish
  its prompt from a builtin waiting for input. Automatic command replay into
  existing panes, including template launch into existing shells, is therefore
  blocked on Windows. Fresh-session creation still supplies its initial command.
  Background jobs cannot be distinguished from foreground jobs by this fallback.
- Space capture quotes commands for the Windows default shell, including paths
  with spaces and arguments with trailing backslashes. Commands containing
  control characters, percent signs, exclamation marks, or embedded double
  quotes are not saved for automatic replay through cmd/PowerShell; they remain
  busy for replay guards and their agents can still receive mail. Native agent
  executable names are case-insensitive and accept the `.exe` suffix.
- Updates stage complete version directories. A write-through replacement of
  one state file selects all six executables. Existing entry-point executables
  forward to that directory; running executable files are never overwritten.
  Rollback swaps current and previous directories. This forwarding retains an
  idle parent process until its child exits.
- Daemon shutdown uses the control protocol first. Forced Windows termination
  uses the native process API. Updating binaries does not restart a daemon or
  replace the image of an already-running host.

## Release evidence

Cross-compilation establishes compilation and linking only. A release needs
native evidence for host rendering and input, ConPTY resize/scroll/split,
attach/detach, persistent sessions, Spaces, mailbox/MCP, clipboard, socket
access from another user, stale endpoint recovery, and update/rollback in a
disposable installation. Record the exact source revision and OS build.
Measure idle CPU, typing latency, resize behavior, and scroll throughput on the
Windows host before advertising performance parity.

Keep release candidates separate from signed macOS and existing immutable
release assets. Publish a new source version only after its platform gates
pass. See [the release process](../release-process.md).
