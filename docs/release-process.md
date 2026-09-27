# Publish release artifacts

The `Release` workflow builds the supported release assets when a `v*` tag is
pushed. It builds Linux x86_64 and ARM64 packages, a native Windows x64 package,
and a signed universal macOS app distributed as a DMG and a zip.

## Prepare a release

1. Set the workspace package version in `Cargo.toml`. The tag version must
   match the binaries. For a prerelease tag such as `v0.2.20-rc.1`, binaries
   may report the base version `0.2.20`.
2. Merge the release source into `main` and complete the release gates in
   [the testing policy](testing-policy.md).
3. Confirm that the repository has the Apple secrets listed below.
4. Push a tag such as `v0.2.20` to start the release workflow.
5. Wait for every build job. The publish job creates the release only after
   every platform package succeeds and the Apple job reports `signed=true`.

The publish job downloads each platform's assets, writes a combined
`SHA256SUMS`, and creates the GitHub release. A tag containing a hyphen creates
a prerelease. The release includes the Linux per-binary updater assets,
manifests, installation archives, the Windows package and manifest, and both
macOS files. The Windows package includes the shared license notices.
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
  -f tag=v0.2.20 \
  -f dry_run=true
gh run list --repo moonbase2090/Prismattyc --workflow release.yml --limit 5
gh run watch RUN_ID --repo moonbase2090/Prismattyc --exit-status
```

Replace `v0.2.20` with the release tag to validate. Copy the run ID from
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
ARM64 runners. They generate manual pages with `scripts/install-man.sh` and
package the result with `scripts/release/package.py`.

The Windows job runs `scripts/release/build-windows.ps1` on Windows Server 2022.
It builds and checks all six Windows executables before creating the release
zip, manifest, and `SHA256SUMS-windows`.

The Apple job runs on `macos-14`. It builds arm64 and x86_64 binaries, checks
the x86_64 binaries' reported versions, combines both architectures, and
packages `Prismattyc.app`. It signs nested code before the app and does not use
recursive signing. The release contains the notarized
`Prismattyc-vVERSION-macos-universal.dmg` and
`Prismattyc-vVERSION-macos-universal.zip`.

Linux x86_64 and ARM64 releases require glibc 2.35 or newer
(Ubuntu 22.04 or newer). The Windows package supports Windows 10 version 1809
or newer, or Windows 11. The macOS app supports macOS 11 or newer.

The command-line updater manages its binary prefix. It does not install or
replace the macOS application bundle.

After publication, check the complete asset list and the combined checksums.
On a disposable Linux installation, run `pmux update --check`, install the
release, test a coordinated restart, and verify rollback. On macOS, open the
zip, validate `Prismattyc.app` with Gatekeeper, and launch it.
