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

That account is a local Windows user created for the proof. It is not the user who owns the development session. A live proof installs Prismattyc, starts `pmuxd`, or opens the host, and those write the per-user PATH, the Run key, `%LOCALAPPDATA%\Programs\Prismattyc`, and the default socket. Issue #307 was proved in a new profile with no Prismattyc install: the MSI ran there, Prismattyc opened there, Create was used with no manual `pmux up`, and `pmux ls` ran in that profile. The owner's PATH, Run key, `%LOCALAPPDATA%`, and socket stay as they were, so an open pmux session keeps running. Issues #310 (Settings opens a config editor), #311 (a same-version reinstall must replace the executables), and #312 (a quiet install still surfaces a `pmuxd` start failure) are live proofs and will use the same account.

The mux ConPTY fixture is `cargo test -p prismattyc-mux --lib`. It does not install Prismattyc, write the Run key, or use the default socket. It does start ConPTY guests in whichever account runs it. Its muse guest is the `ping.exe` that test copied and started. After that process exists, the test opens its handle, checks the volume serial and file index are this run's copy, and terminates that handle on drop. It does not call `taskkill /IM`. Whether this cargo test must also run as the proof account is follow-up #320 and does not block a merge.

- From the repository root in PowerShell, run `./scripts/release/build-windows.ps1 -Output build/verify-windows`. This may install the Rust toolchain and WiX and download dependencies. The output directory must be new. Record the revision and build output.
- Run `./scripts/release/prove-windows-msi.ps1 -PackageDir build/verify-windows -ProofPath build/verify-evidence/msi.txt`. It creates an older package for upgrade proof and invokes the installer lifecycle test. Inspect the recorded install/upgrade/uninstall results, command resolution, and PATH effects. It is a real installation, not a dry-run. Check for residual installation and PATH entries if it fails; retain logs before restoring the test account.
- For PATH maintenance changes, inspect the same-package sequence `0 → 1 → 1 → 0 → 1` in the lifecycle output. Require zero or one cmd entries according to each choice, command resolution when enabled, repair after manually removing the entry while leaving its component registered, and complete uninstall cleanup. A successful first install alone does not prove maintenance honors the current choice.
- A plain install, with `ADDTOPATH` omitted, must prepend one `cmd` entry, write `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\PrismattycPmuxd`, and answer `pmux ls` from a new process whose PATH comes from the registry. The serving `pmuxd` image must be the installed `bin\pmuxd.exe`. `ADDTOPATH=0` must omit the `cmd` entry and still answer through `bin\pmux.exe`. Uninstall must delete the Run value. The lifecycle test stops that installed `pmuxd` before the next file replacement because the installer itself does not stop it. A failed `pmux up` during install is a warning: the files stay and msiexec still succeeds. On a fresh Windows profile, open Prismattyc and use Create with no manual `pmux up`. An elevated install must leave `pmux status` as `running`. `foreign path` means `pmux.sock` is owned by `BUILTIN\Administrators` and a later non-elevated `pmux up` or Create will keep failing until that socket is usable again.
- For cleanup or upgrade-order changes, download the previous released MSI and pass its path as `-ReleasedMsi` to the proof command. Record its release version and SHA-256. Require omitted, zero and one ADDTOPATH upgrades to retain the seeded bin entry and resolve installed commands; the opt-in case must also resolve cmd shims. Require explicit uninstall to remove both command directories from PATH. A synthetic older package contains current cleanup code and cannot prove legacy removal behavior.
- Capture `pmux ls` before and after a newline change in cmd.exe and PowerShell, explicitly recording whether the terminal host is conhost or Windows Terminal. Include the command and several output rows so column alignment is visible. Keep OS/shell/terminal-host versions and both build SHAs with the captures.
- Open a new pane through the desktop UI. Capture it and run `$PSVersionTable.PSVersion` for PowerShell or `echo %COMSPEC%` for cmd.exe to identify the child shell. For shell-selection changes, separately verify configured overrides and fallback availability described by that PR; do not assume an open window proves which shell ran.

## Gotchas

The package proof's explicit PATH option does not alone prove an installer default; test a plain install and `ADDTOPATH=0` when those semantics change. Packaging/installer runs are not isolated by the Unix helper. macOS screenshots and chooser-unit tests are not Windows runtime proof. If the test desktop cannot be reached, report the missing connection and requested exact-tip checks to the coordinator; keep the PR's runtime proof marked outstanding.
