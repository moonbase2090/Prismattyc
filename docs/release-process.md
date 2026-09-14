# Publish release artifacts

`Moonbase2090/Prismattyc` starts at 0.2.0 with fresh git history.
The development repository ancestry is not part of this repository.
Enable immutable releases before publishing the first release.
Protect the default branch, release tags, and publishing credentials.

1. Set the workspace version to the release version.
2. Run the release gates in [the testing policy](testing-policy.md).
3. Build all six binaries on each supported target. Use the oldest supported
   Linux runtime to establish the minimum libc requirement.
4. Package each native build. The packager checks all reported versions.

   ```bash
   python3 scripts/release/package.py --version 0.2.0 \
     --target x86_64-unknown-linux-gnu --bin-dir target/release \
     --out build/release-linux-x86_64
   ```

5. Create a draft release with tag `v0.2.0` in `Moonbase2090/Prismattyc`.
6. Upload every target's six executable assets and manifest to the draft.
7. Publish minimum OS/runtime requirements in the release notes.
8. Verify the complete asset set, sizes, and GitHub SHA-256 metadata.
9. Publish the release. Confirm that GitHub reports it as immutable.
10. Test `pmux update --check`, install, coordinated restart, and rollback
    against a disposable installation. Keep the receipts with the release.

The updater requires exact names such as
`prismattyc-v0.2.0-x86_64-unknown-linux-gnu-pmux`.
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

The initial release supports Linux x86_64 on glibc 2.35 or newer
(Ubuntu 22.04 or newer). Linux arm64 and macOS binary releases are pending.
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
5. Test the archive installer in a clean container before uploading assets.

The package contains individual updater assets and a complete installation
archive. `SHA256SUMS` covers the release assets. The archive contains another
checksum file for its extracted files. The installer verifies these files
before installing them. It leaves existing unrelated installations in place.
