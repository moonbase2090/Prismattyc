# Publish release artifacts

Publish releases in `Moonbase2090/Prismattyc`. Use a new version for each
release. Enable immutable releases before publication.
Protect the default branch, release tags, and publishing credentials.

1. Set the workspace version to the release version.
2. Run the release gates in [the testing policy](testing-policy.md).
3. Build all six binaries on each supported target. Use the oldest supported
   Linux runtime to establish the minimum libc requirement.
4. Generate the manuals with `scripts/install-man.sh`. Set `PRISMATTYC_BINS`
   to the binary directory and `PMUX_MAN_DIR` to `build/release-man`. Package
   each native build. The packager checks all reported versions.

   ```bash
   python3 scripts/release/package.py --version 0.2.8 \
     --target x86_64-unknown-linux-gnu --bin-dir target/release \
     --man-dir build/release-man --out build/release-linux-x86_64
   ```

5. Create a draft release with tag `v0.2.8` in `Moonbase2090/Prismattyc`.
6. Upload every target's six executable assets, manifest, installation archive,
   and checksums to the draft. Upload `MPL-2.0.txt` and `NOTICE.txt` once.
7. Publish minimum OS/runtime requirements in the release notes. State that
   Prismattyc uses MPL-2.0. Link to the license notice and the matching source
   archive, including for users who download individual executables.
8. Verify the complete asset set, sizes, and GitHub SHA-256 metadata. Verify
   that the release tag contains the exact source used to build the binaries.
   Include all covered changes. Confirm that recipients can download the
   source and license notices without authentication.
9. Publish the release. Confirm that GitHub reports it as immutable.
10. Test `pmux update --check`, install, coordinated restart, and rollback
    against a disposable installation. Keep the receipts with the release.

The updater requires exact names such as
`prismattyc-v0.2.8-x86_64-unknown-linux-gnu-pmux`.
It rejects draft, prerelease, mutable, incomplete, or mismatched releases.
Stage all assets before publishing: immutable assets cannot be replaced.
Publish corrections as a new version.

For macOS, sign executable artifacts and publish a signed, notarized
application bundle. The command-line updater manages its binary prefix;
it does not replace an independent application bundle.

GitHub documents [immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)
and [release integrity verification](https://docs.github.com/en/code-security/how-tos/secure-your-supply-chain/secure-your-dependencies/verify-release-integrity).
The updater checks GitHub's HTTPS metadata and digests. It does not claim
independent attestation verification or full TUF protections.

Linux x86_64 and ARM64 releases require glibc 2.35 or newer
(Ubuntu 22.04 or newer). macOS binary releases are pending.
Do not advertise a target until its release assets and platform checks pass.
Apple Silicon source builds have been tested. Mac application distribution
waits for Developer ID signing and notarization.

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
Combine both targets in one draft release. Keep one copy of each shared
license notice. Regenerate `SHA256SUMS` over the combined asset set.

The package contains individual updater assets and a complete installation
archive. `SHA256SUMS` covers the release assets. The archive contains another
checksum file for its extracted files. The installer verifies these files
before installing them. It leaves existing unrelated installations in place.
