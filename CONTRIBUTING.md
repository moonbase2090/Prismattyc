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
changes also need the relevant native fixture under `demo/`; Termwright
runs the nested terminal, not the desktop application.

See [test requirements](docs/testing-policy.md),
[native UI testing](docs/testing-ux.md), and
[acceptance tests](docs/acceptance-pipeline.md) for the remaining checks.

## Run CI locally

The workflows can run through Local Actions. On a machine with limited
memory, use `scripts/la-staged-pr.sh` to run the stages in order. Do not
run heavy coverage, mutation, and native-window jobs concurrently.

A completed job must report a successful status and exit code. Record the
revision you tested and distinguish passing checks from skipped or pending
checks. Keep generated reports in ignored build directories or CI artifacts.

## Version a change

Each PR merged to `main` advances the workspace patch version. Update:

- `Cargo.toml`
- `Cargo.lock`
- `README.md`
- `docs/fidelity-matrix-v1.md`

Run `scripts/check-workspace-version-bumped.sh` before merging. A package
version change does not publish a release or expand terminal compatibility.
See [release packaging](docs/release-process.md) for published builds.

## Write documentation

Use short sentences and plain English. Describe current behavior. Keep
examples runnable and link to the relevant command or configuration guide.
User documentation belongs in `docs/`. Keep internal planning, work logs,
and per-run validation reports out of the product source tree.
