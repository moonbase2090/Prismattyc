# Release smoke matrix

Run this matrix against a release candidate before you tag it, and again
against the published assets. Each row runs in a clean environment for its
platform, installs nothing the owner depends on, and ends with a pass or
fail you can read from a file or log (`smoke.json` on Linux and macOS,
`windows-msi.log` and the noted exit status on Windows). A pass on one row says nothing about another.

| Platform | Environment | Package under test | Command |
| --- | --- | --- | --- |
| macOS | Fresh clone of the pinned Tart base image | `Prismattyc-vVERSION-macos-universal.zip` | [macOS](#macos-tart) |
| Linux | Amazon Linux 2023 container, x86_64 and ARM64 | `prismattyc-vVERSION-<arch>-unknown-linux-gnu-*` executables | [Linux](#linux-docker) |
| Windows | Clean Windows account or VM | `prismattyc-vVERSION-x86_64-pc-windows-msvc.msi` and `.zip` | [Windows](#windows-package) |

A candidate is the build from a dry-run release workflow
(`workflow_dispatch` with `dry_run` on). The published assets are the files
on the GitHub release. Check the published files against `SHA256SUMS`,
`SHA256SUMS-macos` or `SHA256SUMS-windows` first, so the matrix tests what
users download.

## What the shared check covers

`scripts/release/smoke-release.py --bins DIR --out DIR` runs on Linux and macOS.
It uses only the shipped executables, a private daemon and private data
directories. It checks, in order:

1. Every shipped executable reports the same version, and the one you expect
   with `--expect-version`.
2. Real sessions and mail through the production commands.
3. Saved Spaces through the production commands.
4. Update and Restart: a Space of one bare shell and two panes that run
   programs is saved, the daemon restarts with every session stopped and
   restores, and each pane runs its program again rather than a bare shell.
   This repeats once, then the daemon is stopped and the Space is reopened.

`--out` must be new. A pass prints `SMOKE_RELEASE_PASS` and writes
`smoke.json` with the platform, the versions and each step's result; step logs
sit beside it. Keep that directory as the evidence. The run starts no window.
It never uses an installed copy's socket or data.

## Linux (Docker)

The Linux executables are checked for the glibc 2.28 floor and then driven in a
clean Amazon Linux 2023 container. Use a Docker engine of the architecture you
are checking; run it once for x86_64 and once for ARM64.

```sh
mkdir -p smoke/bin
# Download the seven executables for this architecture, drop the
# "prismattyc-vVERSION-<target>-" prefix from each name, and mark them executable.
scripts/release/smoke-linux-docker.sh smoke/bin smoke/linux VERSION
```

Pass: the output ends with `SMOKE_RELEASE_PASS`, the glibc lines stay at or
below 2.28, and `smoke/linux/run/smoke/smoke.json` says `"status": "PASS"`.

## macOS (Tart)

Use a fresh clone of the pinned clean macOS image, one VM at a time, and
delete it when done. Recheck the candidate before you start.

In the guest:

0. Copy these three files from the source tree at the release commit into one
   guest directory, `tools/`: `scripts/release/smoke-release.py`,
   `.cursor/skills/verify-prismattyc/scripts/verify-cli.py` and
   `tests/native/update-restart-e2e.py`. The release package does not carry them.
1. Download the zip and `SHA256SUMS-macos`; run `shasum -a 256 -c` on the zip.
2. Unzip. Run `codesign --verify --strict --deep Prismattyc.app` and
   `spctl --assess --type execute Prismattyc.app`; both must succeed.
3. Run the shared check on the app's executables:

   ```sh
   python3 tools/smoke-release.py \
     --bins Prismattyc.app/Contents/MacOS --out smoke/macos --expect-version VERSION
   ```

4. Open the app from the Dock or Finder. Take a screenshot with a shell pane
   showing, run `pmux versions`, and confirm the host and daemon report the
   same version.
5. For a candidate that is newer than the previous release, install the
   previous release first, then use Update and Restart from the app menu.
   With live sessions, confirm the restart is deferred and those sessions
   keep running; with none, confirm the daemon restarts on the new build.

Pass: every command above succeeds, the app opens to a working pane, and
`smoke.json` says PASS. Record the macOS version and the guest architecture.

## Windows (package)

Use a clean Windows account or VM that has never had Prismattyc installed. Do not use
a developer's installation. The MSI changes that account's PATH, Run key and
per-user program folder.

1. Download the MSI, the zip and `SHA256SUMS-windows`; compare the SHA-256
   values. Copy `scripts/release/test-windows-msi.ps1` from the source tree at
   the release commit into the guest; the package does not carry it. It needs
   no other file.
2. Run the installer lifecycle against the published MSI, with the previous
   release's MSI as the upgrade source:

   ```powershell
   .\test-windows-msi.ps1 -Msi <new.msi> -UpgradeFrom <previous.msi> `
     -ExpectedVersion VERSION *>&1 | Tee-Object smoke\windows-msi.log
   ```

   It installs, upgrades, resolves `pmux` from a new process and uninstalls.
   It throws on the first failure. Its evidence is the exit status, which you
   note, and `smoke\windows-msi.log`.
3. Unzip the zip. Run `--version` on each executable and require one version.
4. On the installed package, open Prismattyc, create a Space with no manual
   `pmux up`, run `pmux ls` in cmd.exe and PowerShell, then use Update and
   Restart and confirm the saved sessions come back running their programs.

Pass: each command exits cleanly, the versions agree, uninstall leaves no
installed files or PATH entry, and the restored panes run their programs.
Detail for each step is in [the Windows verification recipe](../.cursor/skills/verify-prismattyc/features/windows.md).

## Record the result

For each row, keep the platform and OS version, the exact package name and
SHA-256, the commands you ran, and the evidence directory or captures. Post
that summary on the release pull request before the tag. A row you could not
run is reported as not run, with the command you tried and what was missing.
