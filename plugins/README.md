# Immutable plug-in bundles

Each public plug-in version has one archive identity across platforms and core
releases. Build a new version once from a clean source commit, then distribute
that archive unchanged. Native builds consume a verified bundle; they do not
compile guests independently.

`production.json` selects the real guest crates. Their manifests own plug-in
IDs, versions, capabilities, files, and contributions. Runtime fixtures are
excluded from the producer.

## First unpublished candidate

From a clean checkout with Rust 1.90.0 and its `wasm32-unknown-unknown` target:

```sh
python3 scripts/release/build-plugin-release.py \
  --bootstrap --output target/plugin-candidate
```

The producer builds both guests for one target, component-encodes and validates
them, and writes digest-named archives, `pmux-plugins.json`, a build receipt,
and `bundle-pin.json`. The receipt records each archive's source commit,
source tree fingerprint, lockfile digest, and toolchain. No machine paths or
filesystem metadata enter those records. Repeating the same completed build
reuses its verified output.

Prepare that candidate for a private native build:

```sh
export PRISMATTYC_PLUGIN_BUNDLE_DIR="$(python3 scripts/release/prepare-plugin-bundle.py \
  --candidate target/plugin-candidate)"
cargo +1.90.0 build --release --locked -p prismattyc-mux --bins
cargo +1.90.0 build --release --locked --manifest-path tools/activity-monitor/Cargo.toml
```

Candidate preparation is explicit. It copies only pinned files, verifies the
result again, and caches it by the full reviewed pin. This does not install or enable
plug-ins, grant access, or start their workers.

## Retain existing versions

For a later catalog, supply the latest official verified bundle:

```sh
python3 scripts/release/build-plugin-release.py \
  --previous /absolute/path/to/verified-bundle --output target/next-candidate
```

Every previous version and its original build record is retained. A selected
ID and version already present in that bundle reuses its exact archive. Changed
sources under that same version fail with instructions to bump the manifest
version. Shared core changes alone do not rebuild an unchanged guest crate.
`--previous-candidate` permits the same reuse check with private unpublished
artifacts; it is a proof option, not publication authority.

## Official catalog pins

The owner publishes the already reviewed bytes in a separate GitHub release
named `pmux-plugins-vSEMVER`, with prerelease and immutable release settings
enabled and latest disabled. It contains the catalog, every named archive,
and `plugin-build-receipt.json`. Core stable release selection remains separate.
These tools have no tag, upload, or publication command.

After that release exists, generate a pin from its actual metadata:

```sh
cargo +1.90.0 run --release --locked -p prismattyc-mux --example plugin_bundle -- \
  pin pmux-plugins-v1.0.0 target/official-plugin-bundle
```

Review the generated `bundle-pin.json` before checking it in at
`plugins/bundle-pin.json`. The pin binds the exact catalog and build receipt.
Catalog entries bind every archive. Fetch validates the fixed official source,
exact tag, publication flags, asset URLs, sizes, and digests. It never selects
whichever catalog happens to be newest during a normal core build.

Once that reviewed pin exists, normal preparation is:

```sh
export PRISMATTYC_PLUGIN_BUNDLE_DIR="$(python3 scripts/release/prepare-plugin-bundle.py)"
```

A verified cache works offline. A different pin requires its own verified
bundle. Until the first official catalog has been published and reviewed,
this repository deliberately has no official pin. Use explicit candidates
for private proof and CI; do not invent an unpublished official tag.

## Check packaged consumers

Native build scripts prepare the reviewed pin, or reuse an explicit
`PRISMATTYC_PLUGIN_BUNDLE_DIR` for private candidate builds. The local macOS app
builder and installer include all seven executables, including the thin monitor
launcher. Packaging verifies the five plug-in consumers before it writes output:

```sh
python3 scripts/release/plugin_bundle_provenance.py verify --bins target/release
```

Each consumer's standalone `--plugin-bundle` command reports its physical
executable's embedded catalog, pin, build receipt, package digests, and core
revision. It does not follow updater forwarding, connect to the daemon, or run a
guest. Packaging rejects a mismatched helper even when its core version matches.
The app resources, Linux installation, and Windows package carry
`plugin-bundle.json` with that identity and the original build receipt.
