# Publish release artifacts

The `Release` workflow builds the supported release assets when a `v*` tag is
pushed. It builds Linux x86_64 and ARM64 packages, a native Windows x64 package,
and a signed universal macOS app distributed as a DMG and a zip.

## Prepare a release

1. Open the release PR on a `release/vX.Y.Z-rc.N-changelog` branch. Set
   the version in all four files: the `[workspace.package]` version in
   `Cargo.toml`, then `cargo update --workspace` to refresh only the
   workspace entries in `Cargo.lock`, then the README and fidelity-matrix
   sentences. Add the `## [X.Y.Z]` and `## [X.Y.Z-rc.N]` CHANGELOG sections,
   and run `scripts/check-workspace-version.sh`. The tag version must match
   the binaries. For a prerelease tag such as `v0.2.21-rc.1`, binaries may
   report the base version `0.2.21`. A later rc or the final release that
   keeps the same base version changes only the CHANGELOG, not the version.
2. Merge the release source into `main` and complete the release gates in
   [the testing policy](testing-policy.md).
3. Confirm that the repository has the Apple secrets listed below.
4. Push a tag such as `v0.2.21` to start the release workflow.
5. Wait for every build job. The publish job creates the release only after
   every platform package succeeds and the Apple job reports `signed=true`.

The publish job downloads each platform's assets, writes a combined
`SHA256SUMS`, and creates the GitHub release. It copies the matching
`CHANGELOG.md` section into the release body and fails if that section is
missing. A tag containing a hyphen is published with `--prerelease --latest=false`,
so it does not become the Latest release. `pmux update` and the app menu ignore
it unless the user passes `--pre`. The prismattyc.com download picker lives in
`moonbase2090/prismattyc-website`; it skips drafts, prereleases, and any tag
that is not `vX.Y.Z`. The release includes the Linux per-binary updater assets,
manifests, installation archives, the Windows package and manifest, both
macOS files, `SHA256SUMS-macos`, and `manifest-macos-universal.json`.
The Windows package includes the shared license notices.
Platform checksum files remain alongside the combined checksum. GitHub also
provides source archives for the tag.

## Run a signed dry run

Set `dry_run` to `true` to build, sign, and notarize all platform assets without
publishing a release. Dispatch the workflow from `main` and choose a tag whose
base version matches the checked-out binaries. A dry run does not create or
push a tag.

```bash
gh workflow run release.yml \
  --repo moonbase2090/Prismattyc \
  --ref main \
  -f tag=v0.2.21 \
  -f dry_run=true
gh run list --repo moonbase2090/Prismattyc --workflow release.yml --limit 5
gh run watch RUN_ID --repo moonbase2090/Prismattyc --exit-status
```

Replace `v0.2.21` with the release tag to validate. Copy the run ID from
`gh run list` into `gh run watch`.

## Configure Apple credentials

The Apple job requires five repository secrets:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE_P12` | Base64-encoded Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | Password for the `.p12` |
| `APPLE_NOTARY_ISSUER` | App Store Connect issuer ID |
| `APPLE_NOTARY_KEY_ID` | App Store Connect API key ID |
| `APPLE_NOTARY_KEY` | App Store Connect `.p8` PEM or base64-encoded PEM |

The workflow imports the certificate into a temporary keychain under a
`mktemp` directory and removes it when the job exits. It signs the helper
binaries and app with the hardened runtime and
`scripts/release/prismattyc.entitlements`. Prismattyc currently needs no
special hardened runtime entitlements. The workflow submits the app archive
and DMG to Apple's notary service, then staples and validates the app and DMG.

## Build platform packages

The Linux jobs use Ubuntu 22.04 and build all six binaries on native x86_64 or
ARM64 runners. `scripts/release/build-linux.sh` uses `cargo-zigbuild` with a
`.2.28` target suffix, while keeping the existing target triples and asset
names. The jobs run `scripts/release/smoke-linux-al2023.sh` before packaging.
That check reports the maximum GLIBC symbol version from `objdump -T` for each
binary, then runs `prismattyc --version`, `pmux --version`, and
`pmuxd --version` in `amazonlinux:2023`. CI runs the same check for both
architectures. The jobs generate manual pages with `scripts/install-man.sh`
and package the result with `scripts/release/package.py`.

The Windows job runs `scripts/release/build-windows.ps1` on Windows Server 2022.
It builds and checks all six Windows executables before creating the release
zip, manifest, and `SHA256SUMS-windows`.

The Apple job runs on `macos-26` (stable GitHub-hosted image with Xcode 26 and
macOS SDK 26+; arm64, with x86_64 produced by cross-compile). It builds arm64
and x86_64 binaries against a current macOS SDK so AppKit can draw current
window chrome, keeps `MACOSX_DEPLOYMENT_TARGET=11.0`, checks the x86_64
binaries' reported versions, combines both architectures, and packages
`Prismattyc.app`. It signs nested code before the app and does not use
recursive signing. The release contains the notarized
`Prismattyc-vVERSION-macos-universal.dmg` and
`Prismattyc-vVERSION-macos-universal.zip`.

Linux x86_64 and ARM64 release binaries require glibc 2.28 or newer, including
Amazon Linux 2023. The Windows package supports Windows 10 version 1809 or
newer, or Windows 11. The macOS app supports macOS 11 or newer.

On Linux and Windows, `pmux update` replaces the six installed binaries.
On macOS it downloads the universal zip, verifies `SHA256SUMS-macos`, the
macOS manifest when published, the Developer ID Team ID, and Gatekeeper
notarization before replacing `Prismattyc.app`. It keeps the previous app for
rollback. Releases from v0.2.21 onward can be installed this way.
`prismattyc update` runs the same installer.

After publication, check the complete asset list and the combined checksums.
On a disposable Linux installation, run `pmux update --check`, install the
release, test a coordinated restart, and verify rollback. On macOS, run
`pmux update --check`, install the release from the app menu, and confirm the
host reconnects to its saved pmux views. When the daemon has no active
sessions, confirm it restarts on the new build. With live sessions, confirm
the restart is deferred and those sessions continue. Run
`pmux update --rollback` and verify that the previous signed app is restored.
