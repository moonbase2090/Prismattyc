# Build and test Prismattyc

Use Rust 1.90 or newer. Keep dependency changes in `Cargo.lock` and use
`--locked` when you validate a change.

## Check a change

Run these commands from the repository root:

```bash
cargo fmt --all --check
cargo check --workspace --locked
cargo test --workspace --locked -- --test-threads=1
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Use one test thread for the full workspace. Some tests start real processes
and depend on timing. Preserve any existing work in the checkout.

For terminal input or rendering changes, also run:

```bash
./scripts/termwright-e2e.sh
```

Inspect the generated PNG files under `e2e/artifacts/`. Desktop-window
changes also need the relevant native fixture under `tests/native/`; Termwright
runs the nested terminal, not the desktop application.

See [test requirements](docs/testing-policy.md),
[native UI testing](docs/testing-ux.md), and
[acceptance tests](docs/acceptance-pipeline.md) for the remaining checks.

Every test must fail when the behavior it covers breaks. AGENTS.md lists
the tautological tests that review rejects.

## Run CI locally

The workflows can run through Local Actions. On a machine with limited
memory, use `scripts/la-staged-pr.sh` to run the stages in order. Do not
run heavy coverage, mutation, and native-window jobs concurrently.

A completed job must report a successful status and exit code. Record the
revision you tested and distinguish passing checks from skipped or pending
checks. Keep generated reports in ignored build directories or CI artifacts.

The Jev PR triage workflow is advisory only: it can add `triage:*` labels and one comment, and it never blocks a merge.

## Version a change

Feature and fix PRs leave the workspace version alone in all four files:

- `Cargo.toml`
- `Cargo.lock`
- `README.md`
- `docs/fidelity-matrix-v1.md`

If a branch picked up a version bump, restore those four files to the base's
version. Run `scripts/check-workspace-version.sh` before merging. Only the
release PR (`release/vX.Y.Z-rc.N-changelog`, which adds the `## [X.Y.Z]` and
`## [X.Y.Z-rc.N]` CHANGELOG sections before the signed tag) moves the
version, and it updates all four files together. The workspace version is
always a plain `X.Y.Z` with no prerelease suffix. A package version change
does not publish a release or expand terminal compatibility. See
[release packaging](docs/release-process.md) for published builds.

## Write documentation

Use short sentences and plain English. Describe current behavior. Keep
examples runnable and link to the relevant command or configuration guide.
User documentation belongs in `docs/`. Keep internal planning, work logs,
and per-run validation reports out of the product source tree.

## License contributions

Contribute original code and documentation under MPL-2.0. The repository
[license](LICENSE) and [notice](NOTICE.txt) describe its scope. Add
`SPDX-License-Identifier: MPL-2.0` in a comment when you create a source
file. Preserve existing third-party license and copyright notices.
