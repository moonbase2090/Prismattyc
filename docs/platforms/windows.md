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
and writes a per-user MSI, a ZIP, individual updater assets, source revision
metadata, licenses, and SHA-256 checksums to `build/windows-release`. Use
`-Output PATH` for another new output directory. The default target is
`x86_64-pc-windows-msvc`. The GNU target additionally requires a native MinGW
toolchain on PATH. WiX 5.0.2 compiles the MSI and is installed with
`dotnet tool install --global wix --version 5.0.2` when it is not already on
PATH. WiX's directory-name check fails on macOS and Linux, so the MSI is built
on Windows.

## Install the MSI

The release asset is `prismattyc-vVERSION-x86_64-pc-windows-msvc.msi`. It is a
per-user package: it does not request administrator rights. It installs the six
executables, licenses, and `NOTICE` under
`%LOCALAPPDATA%\Programs\Prismattyc`, with the executables in `bin`. A Start
menu shortcut named Prismattyc launches `prismattyc-host.exe`. Apps & features
shows the publisher Moonbase2090 and the numeric MSI product version described
below. `pmux --version` still reports the semver.

```powershell
msiexec /i prismattyc-v0.3.29-rc.2-x86_64-pc-windows-msvc.msi /qn /norestart
msiexec /i prismattyc-v0.3.29-rc.2-x86_64-pc-windows-msvc.msi /qn /norestart ADDTOPATH=1
```

`ADDTOPATH=1` prepends the install `bin` directory to the user PATH. The
default is off, matching `install-windows-preview.ps1 -AddToPath`. Uninstall
removes that PATH entry, the files, and the shortcut. It leaves `%APPDATA%`
configuration and `%LOCALAPPDATA%\Prismattyc` data in place, including the
update store under `%LOCALAPPDATA%\prismattyc\updates`.

MSI `ProductVersion` has three numbers of at most 65535 and cannot store a
prerelease tag. Patch numbers through 64 map to `patch * 1000 + slot`. A
stable `X.Y.Z` uses slot 999. `X.Y.Z-rc.N` uses slot N, from 1 through 998.
`0.3.29-rc.2` is product version `0.3.29002`, `0.3.29` is `0.3.29999`, and
`0.3.30-rc.1` is `0.3.30001`. The UpgradeCode
`681faa33-88f2-5c22-aeba-86ab1fffd4da` stays fixed so a newer MSI replaces the
older one. Other prerelease spellings are rejected.

The installer does not stop a running daemon or host. Restart Manager is
disabled. If an executable in the install directory is still running, Windows
cannot replace that file and `msiexec` fails the copy; the process keeps
running. Close Prismattyc before installing. `pmux update` stores new builds
under `%LOCALAPPDATA%\prismattyc\updates` and the installed executables forward
to the selected version on their next launch. An MSI install runs
`reset-windows-update-pointer.ps1`, which deletes `windows-current.json` when
it exists, so the next launch runs the MSI binaries instead of an older staged
update. The script does not kill or restart any process.

`scripts/release/test-windows-msi.ps1` installs with `msiexec /qn`, checks the
files, shortcut, Apps & features entry, and `--version`, upgrades from an
older MSI when one is passed, uninstalls, and checks that the install is gone
while the preserved data files remain.

Extract the ZIP into a directory you own when you are not using the MSI.
Launch `bin\prismattyc-host.exe`, or add its `bin` directory to your user PATH.
After that, `pmux update` or `prismattyc update` installs a newer published
release and leaves running programs on the old version until they restart. The
default shell is `%COMSPEC%`, normally `cmd.exe`. Select PowerShell with
`-- powershell.exe` or `-- pwsh.exe`.

## Code signing

The Windows release job signs the six executables before packaging, then signs
the MSI, when all of these repository variables are set:

| Variable | Purpose |
| --- | --- |
| `AZURE_TENANT_ID` | Entra tenant ID |
| `AZURE_CLIENT_ID` | App registration used by GitHub OIDC |
| `AZURE_SUBSCRIPTION_ID` | Subscription that holds the signing account |
| `AZURE_ARTIFACT_SIGNING_ENDPOINT` | Regional endpoint, such as `https://eus.codesigning.azure.net` |
| `AZURE_ARTIFACT_SIGNING_ACCOUNT` | Artifact Signing account name |
| `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE` | Certificate profile name |

If any variable is missing, the job logs `Windows code signing skipped` and
publishes an unsigned MSI and ZIP. A missing account is not a build failure.
The manifest field `signing` is `unsigned` or `azure-artifact-signing`. When
signing is on, the job runs `signtool verify /pa` on the packaged executables
and the MSI. Signatures use SHA-256 and the RFC 3161 timestamp at
`http://timestamp.acs.microsoft.com`.

Authentication is OpenID Connect through `azure/login` and
`azure/artifact-signing-action`. There is no client secret in the workflow.
Create the account, complete identity validation, and create a certificate
profile in the Azure portal. Microsoft currently offers Artifact Signing to
organizations in the United States and Canada with at least three years of
verifiable history. Assign the app registration the Artifact Signing
Certificate Profile Signer role on that profile. Add a federated credential
whose issuer is `https://token.actions.githubusercontent.com`, whose subject
is `repo:moonbase2090/Prismattyc:ref:refs/tags/v*`, and whose audience is
`api://AzureADTokenExchange`. Workflow-dispatch builds of a tag need a second
credential only if the subject does not match the tag ref. Do not commit the
values.

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
