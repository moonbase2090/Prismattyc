# Windows installation and terminal behavior

Windows users install the per-user package, run commands in their console, and open desktop panes with the selected shell.

## Sub-features

- `installer`: install, upgrade, command resolution, and uninstall.
- `console-newline`: line-oriented output starts each line at column zero.
- `new-pane-shell`: a new pane runs the configured or automatically selected shell.

## How to get to it (user POV)

Run the MSI, launch Prismattyc from Start, run `pmux ls` in cmd.exe or PowerShell inside conhost and Windows Terminal, and create a new pane in the host.

## Driving it with PowerShell and native captures

Preconditions: an actual Windows desktop or disposable test VM, an account dedicated to installer verification, Rust/MSVC build tools, Python, .NET, and WiX. Use a clean account without an existing Prismattyc installation; the installer test changes that account's installed application and PATH. Unit tests on another OS do not satisfy this recipe.

- From the repository root in PowerShell, run `./scripts/release/build-windows.ps1 -Output build/verify-windows`. This may install the Rust toolchain and WiX and download dependencies. The output directory must be new. Record the revision and build output.
- Run `./scripts/release/prove-windows-msi.ps1 -PackageDir build/verify-windows -ProofPath build/verify-evidence/msi.txt`. It creates an older package for upgrade proof and invokes the installer lifecycle test. Inspect the recorded install/upgrade/uninstall results, command resolution, and PATH effects. It is a real installation, not a dry-run. Check for residual installation and PATH entries if it fails; retain logs before restoring the test account.
- For cleanup or upgrade-order changes, download the previous released MSI and pass its path as `-ReleasedMsi` to the proof command. Record its release version and SHA-256. Require omitted, zero and one ADDTOPATH upgrades to retain the seeded bin entry and resolve installed commands; the opt-in case must also resolve cmd shims. Require explicit uninstall to remove both command directories from PATH. A synthetic older package contains current cleanup code and cannot prove legacy removal behavior.
- Capture `pmux ls` before and after a newline change in cmd.exe and PowerShell, explicitly recording whether the terminal host is conhost or Windows Terminal. Include the command and several output rows so column alignment is visible. Keep OS/shell/terminal-host versions and both build SHAs with the captures.
- Open a new pane through the desktop UI. Capture it and run `$PSVersionTable.PSVersion` for PowerShell or `echo %COMSPEC%` for cmd.exe to identify the child shell. For shell-selection changes, separately verify configured overrides and fallback availability described by that PR; do not assume an open window proves which shell ran.

## Gotchas

The package proof's explicit PATH option does not alone prove an installer default; test a plain install and `ADDTOPATH=0` when those semantics change. Packaging/installer runs are not isolated by the Unix helper. macOS screenshots and chooser-unit tests are not Windows runtime proof. If the test desktop cannot be reached, report the missing connection and requested exact-tip checks to the coordinator; keep the PR's runtime proof marked outstanding.
