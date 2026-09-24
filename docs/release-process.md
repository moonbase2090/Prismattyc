# Publish release artifacts

Publish releases in `Moonbase2090/Prismattyc`. Use a new version for each
release. Enable immutable releases before publication.
Protect the default branch, release tags, and publishing credentials.

Immutable releases cannot gain assets after publication. Build the Linux
archives and the Apple silicon app first. Upload every asset to one draft,
verify that set, and only then publish. Do not publish a Linux-only release
and add macOS later. Do not create a companion tag for the Mac build.

1. Set the workspace version to the release version.
2. Run the release gates in [the testing policy](testing-policy.md).
3. Build all six binaries on each supported Linux target. Use the oldest
   supported Linux runtime to establish the minimum libc requirement.
4. Generate the manuals with `scripts/install-man.sh`. Set `PRISMATTYC_BINS`
   to the binary directory and `PMUX_MAN_DIR` to `build/release-man`. Package
   each Linux build. The packager checks all reported versions.

   ```bash
   python3 scripts/release/package.py --version 0.2.8 \
     --target x86_64-unknown-linux-gnu --bin-dir target/release \
     --man-dir build/release-man --out build/release-linux-x86_64
   ```

   Repeat for `aarch64-unknown-linux-gnu` with its own `--bin-dir` and `--out`.

5. On a Mac, build, sign, notarize, and staple the Apple silicon app for the
   same version. The asset name is `Prismattyc-v0.2.8-macos-arm64.zip`.
   See [Build the macOS release](#build-the-macos-release). Do not build an
   Intel or universal zip.
6. Copy the Linux packager outputs and the macOS zip into one directory.
   Leave out each packager directory's own `SHA256SUMS`. Keep one copy of
   `MPL-2.0.txt` and `NOTICE.txt`. Write a new checksum file over that
   combined set. The checksum line format matches the Linux packager.

   ```bash
   mkdir -p build/release-upload
   find build/release-linux-x86_64 build/release-linux-aarch64 \
     -maxdepth 1 -type f ! -name SHA256SUMS \
     -exec cp {} build/release-upload/ \;
   cp build/release-macos-arm64/Prismattyc-v0.2.8-macos-arm64.zip \
     build/release-upload/
   python3 scripts/release/write-sha256sums.py build/release-upload
   ```

7. Create a draft release with tag `v0.2.8` in `Moonbase2090/Prismattyc`.
   Upload every file in `build/release-upload`, including
   `Prismattyc-v0.2.8-macos-arm64.zip` and `SHA256SUMS`, while the release
   is still a draft.
8. Publish minimum OS/runtime requirements in the release notes. State that
   Prismattyc uses MPL-2.0. Link to the license notice and the matching source
   archive, including for users who download individual executables. For the
   Mac asset, state Apple silicon and macOS 11 or newer, and use the zip name
   above.
9. Verify the complete asset set, sizes, and GitHub SHA-256 metadata while
   the release is still a draft. The draft must list the macOS zip when this
   release ships macOS. Verify that the release tag contains the exact source
   used to build the binaries. Include all covered changes. Confirm that
   recipients can download the source and license notices without
   authentication.
10. Publish the release. Confirm that GitHub reports it as immutable.
11. Test `pmux update --check`, install, coordinated restart, and rollback
    against a disposable Linux installation. On an Apple silicon Mac, open
    the zip, confirm Gatekeeper accepts `Prismattyc.app`, and launch it.
    Keep the receipts with the release.

The updater requires exact names such as
`prismattyc-v0.2.8-x86_64-unknown-linux-gnu-pmux`.
It rejects draft, prerelease, mutable, incomplete, or mismatched releases.
The macOS app asset name is `Prismattyc-v0.2.8-macos-arm64.zip`.
`pmux update` does not install or replace that app.
Stage all assets before publishing: immutable assets cannot be replaced.
Publish corrections as a new version.

For macOS, ship the signed and notarized application bundle produced by
`scripts/release/package-macos.sh`. The command-line updater manages its
binary prefix; it does not replace an independent application bundle.

GitHub documents [immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)
and [release integrity verification](https://docs.github.com/en/code-security/how-tos/secure-your-supply-chain/secure-your-dependencies/verify-release-integrity).
The updater checks GitHub's HTTPS metadata and digests. It does not claim
independent attestation verification or full TUF protections.

Linux x86_64 and ARM64 releases require glibc 2.35 or newer
(Ubuntu 22.04 or newer). The macOS download is an Apple silicon app for
macOS 11 or newer. Do not advertise Intel or a universal binary.
Do not advertise a target until its release assets and platform checks pass.

## Build the Linux release

1. Build the Ubuntu 22.04 image:

   ```bash
   docker build -t prismattyc-release:ubuntu22 -f scripts/release/Dockerfile.linux .
   ```

2. Build all six binaries with the pinned Rust 1.90.0 toolchain in that image.
3. Generate the manuals with `scripts/install-man.sh`. Set `PRISMATTYC_BINS`
   to the binary directory. Set `PMUX_MAN_DIR` to a staging directory.
4. Run `scripts/release/package.py` in the build image. Supply `--version`,
   `--target`, `--bin-dir`, `--man-dir`, and a new `--out` directory.
5. Test the archive installer in a clean container for each architecture
   before uploading assets. Run the ARM64 checks on ARM64 hardware or under
   emulation. Record which environment you used.

Use target `aarch64-unknown-linux-gnu` when packaging ARM64 binaries. The
packager rejects executable files for a different architecture. Run it in
an environment that can execute the binaries, including through emulation.
Combine both Linux targets and the macOS zip in one draft release. Keep one
copy of each shared license notice. Regenerate `SHA256SUMS` over the combined
asset set, including `Prismattyc-vX.Y.Z-macos-arm64.zip` when shipping macOS.

The package contains individual updater assets and a complete installation
archive. `SHA256SUMS` covers the release assets. The archive contains another
checksum file for its extracted files. The installer verifies these files
before installing them. It leaves existing unrelated installations in place.

## Build the macOS release

Run this on a Mac that has the Developer ID Application certificate in the
login keychain and a `notarytool` keychain profile. The script builds the
arm64 host and the mux helpers, assembles `Prismattyc.app`, signs each nested
Mach-O and then the app with the hardened runtime and a secure timestamp,
submits a zip to Apple, staples the ticket, and checks Gatekeeper.

The first notarization for a team can take hours. Later submissions are
usually minutes. The script waits up to six hours. Set
`PRISMATTYC_NOTARY_TIMEOUT` to another limit, such as `30m` or `2h`, when
you already know the queue is moving.

One-time credential setup. `notarytool` prompts for the app-specific password
and stores it in the keychain. Do not commit that password, an API key, or
a certificate, and do not pass the password on the command line.

```bash
xcrun notarytool store-credentials moonbase-notary \
  --apple-id "$APPLE_ID" \
  --team-id "$APPLE_TEAM_ID"
```

The default signing identity is
`Developer ID Application: Moonbase 2090 LLC (S24C53PD3Y)`.
Override it with `PRISMATTYC_CODESIGN_IDENTITY`.
The default keychain profile name is `moonbase-notary`.
Override it with `PRISMATTYC_NOTARY_KEYCHAIN_PROFILE`.

```bash
scripts/release/package-macos.sh --version 0.2.8 \
  --out build/release-macos-arm64
```

`python3 scripts/release/package.py --version 0.2.8 --target aarch64-apple-darwin --out build/release-macos-arm64`
runs the same script. `--target x86_64-apple-darwin` fails until a real
Intel or universal build exists. Pass `--bin-dir` when the four arm64
binaries are already built: `prismattyc-host`, `pmux`, `pmuxd`, and
`pmux-attach`. Pass `--app` to sign and notarize an existing
`Prismattyc.app` instead of assembling one.

The output directory contains `Prismattyc-v0.2.8-macos-arm64.zip` and a
`SHA256SUMS` for that directory only. Upload the zip. The checksum file
that ships is the combined one from the draft directory, and it must list
the zip. The release zip is the stapled app. It is not the archive that
was uploaded to Apple before stapling. AppleDouble `._*` members and
`__MACOSX` entries are removed from the zip, and the script checks the
extracted app again.

Copy that zip to the machine that uploads the draft before publishing.
A failed notarization leaves a work directory and prints a `--app` retry
command so the signed bundle does not have to be rebuilt.
