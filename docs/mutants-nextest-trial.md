# Trial nextest for PR mutation tests

Track this experiment in [issue #354](https://github.com/brandanmajeske/Prismattyc/issues/354).

The PR gate still uses Cargo. This opt-in trial compares Cargo with nextest on ten pinned PR349 mutants. It does not calculate a gate score or replace the complete mutation universe.

## Measured result

**Keep Cargo as the default.** Nextest preserved all ten results but did not improve total time in this sample. The helper remains an opt-in experiment. The 80% PR gate and its complete-universe accounting are unchanged.

The paired run used the same pinned source `270d5548fd38333d1c1e927d5a27b6f9f545e1c7`, Rust 1.90.0, cargo-mutants 27.1.0, and an isolated LocalActions runner container. Nextest was 0.9.143. The helper was `91a4660210123529cdd1fe165287be77b4b11de5`. Cargo ran first, then nextest. Both baselines passed all 730 tests. Each tool reproduced five caught and five missed results, with no timeout or unviable result.

| Measurement | Cargo seconds | Nextest seconds | Nextest change |
|---|---:|---:|---:|
| Ten mutant builds | 48.121 | 33.313 | -30.77% |
| Ten mutant test executions | 869.930 | 888.430 | +2.13% |
| Ten mutants, build plus test | 918.050 | 921.743 | +0.40% |
| Entire invocation, including baseline and overhead | 1050.748 | 1052.433 | +0.16% |

The caught group used 3.91% less build-plus-test time. Its test time alone fell by only 0.91%. The missed group used 4.87% more build-plus-test time. Some apparent savings came from build variation, not fail-fast. This is one ordered pair on a deliberately selected set, not a representative speed benchmark or a mutation gate score. Do not infer a stable improvement from these small differences.

Fail-fast worked mechanically. For example, the `open_space_from_host` body replacement stopped after 514 of 730 tests. That case used 82.913 seconds testing under nextest versus 85.581 seconds under Cargo. Its first failure came late enough that avoiding the remaining tests saved little.

## Inspect the paired identities

Each duration below is a fresh phase result in seconds. `C` means caught; `M` means missed. Both tools matched the retained result from the cancelled PR349 run `1789052743-6354652e`. The retained observations selected the sample; they are not used as the timing control.

| Mutant in `crates/prismattyc-host/src/main.rs` | Result on both | Cargo build | Cargo test | Nextest build | Nextest test |
|---|---|---:|---:|---:|---:|
| `6563:5: replace open_space_from_host with ()` | C | 10.546 | 85.581 | 3.657 | 82.913 |
| `6633:9: replace && with \|\| in open_space_from_host` | C | 11.021 | 85.009 | 3.506 | 82.867 |
| `6632:9: replace && with \|\| in open_space_from_host` | C | 2.955 | 87.017 | 2.855 | 82.768 |
| `6633:47: replace == with != in open_space_from_host` | C | 2.855 | 80.663 | 2.855 | 82.661 |
| `6645:5: replace advance_space_opens with ()` | C | 3.509 | 97.791 | 3.707 | 100.888 |
| `2567:9: replace App::pump with ()` | M | 4.207 | 86.418 | 4.060 | 90.973 |
| `2582:17: replace && with \|\| in App::pump` | M | 4.559 | 86.674 | 4.307 | 91.331 |
| `2581:17: replace && with \|\| in App::pump` | M | 2.805 | 86.471 | 2.755 | 91.478 |
| `2580:16: delete ! in App::pump` | M | 2.957 | 86.719 | 2.856 | 91.074 |
| `2582:20: delete ! in App::pump` | M | 2.706 | 87.587 | 2.755 | 91.478 |

## Verify the receipt

- Evidence root: `/home/brandan/.cache/prismattyc/pt354-evidence/paired-001/`.
- `comparison.json`: `complete_pair=true`, `all_results_match=true`.
- `cargo-run.json` and `nextest-run.json`: exit 2 on both, as expected for missed mutants; cgroup OOM counters remain 0.
- `cargo/mutants.out/outcomes.json` and `nextest/mutants.out/outcomes.json`: complete raw outcomes, baselines, precise phase durations, and process arguments.
- `plan.json`: original snapshot SHA-256, selected identities, source fingerprint, and commands.
- `versions.json`: Rust, Cargo, mutation tool, nextest, and memory-cap versions.
- The outer driver exited 0. The source fingerprint matched before and after each path.

The container had an 8 GiB memory cap and no container swap allowance. The observed peak was 6.18 GiB; the memory-limit and OOM counters were zero at the final observation. The container stopped and both shared locks were released after the run. Host headroom passed before each path. This direct runner experiment is not a Local Actions daemon merge-gate receipt.

Seven helper regression tests also pass. A standalone real-tool fixture proves both catches and first-failure termination. An empty nextest selection exits 4 and is rejected as infrastructure failure.

The PR was then rebased onto main `2c272399` for review. These timings remain bound to the historical source and helper heads above; they are not a new-main benchmark.

## Run the comparison

1. Obtain a runner window from the swarm lead. Keep Windows retries free.
2. Use an isolated checkout at `270d5548fd38333d1c1e927d5a27b6f9f545e1c7`.
3. Retain the PT351 `original-artifacts.json` snapshot. The helper checks its source fingerprint against the checkout.
4. Provide cargo-mutants 27.1.0 and nextest 0.9.143. Use the same Rust toolchain for both paths.
5. Use the Local Actions runner with an 8 GiB memory cap and durable SSD output. Expose host memory information as for the existing mutants job.
6. Create a plan without running a build:

```bash
python3 scripts/mutants-nextest-trial.py \
  --snapshot /path/to/original-artifacts.json \
  --repo /path/to/pinned-349-checkout \
  --output /path/to/new-plan-directory
```

7. Inspect `plan.json`. Confirm five original catches and five original misses.
8. Repeat the command with a new output directory and `--run` during the assigned window. The helper acquires the normal heavy-job lock without waiting. It refuses inadequate headroom, a memory cap above 8 GiB, a stale source fingerprint, or an unavailable nextest version.
9. Inspect `comparison.json`, both baseline outcomes, and the raw logs. Require `complete_pair` and `all_results_match` before reporting a successful comparison. A single-tool run is partial evidence.

The default order is Cargo, then nextest. Use `--tools nextest cargo` in a separate authorized run to test order effects. Each path runs its own baseline. Phase durations exclude baseline and orchestration; the `*-run.json` files also record total wall time. Do not compare a historical duration with a fresh duration as a paired speedup.

Retain original outcome files, discovery, versions, command exits, cgroup OOM counters, and all ten results. A changed caught/missed result needs investigation before any gate rollout. A failed baseline or incomplete identity set invalidates the trial.

## Keep nextest execution serial

The helper uses the supported [cargo-mutants nextest integration](https://mutants.rs/nextest.html). It passes `--jobs 1` to cargo-mutants and sets `CARGO_BUILD_JOBS=1`. Nextest runs one test process at a time with no retries:

```bash
cargo mutants --test-tool nextest --jobs 1 \
  --cargo-test-arg=--test-threads=1 \
  --cargo-test-arg=--fail-fast \
  --cargo-test-arg=--retries=0 \
  --cargo-test-arg=--no-tests=fail \
  --cargo-test-arg=--ignore-default-filter \
  --cargo-test-arg=--profile=default \
  --cargo-test-arg=--user-config-file=none \
  -p prismattyc-host -- --locked
```

This example shows argument placement. Use the trial helper to restrict execution to the ten identities. Do not run the example as an unrestricted host mutation job.

`--fail-fast` conflicts with nextest `--no-run`. Pass it through `--cargo-test-arg`, so cargo-mutants adds it only in the Test phase. Do not pass it as a general Cargo argument. Nextest starts a [separate process for each test](https://nexte.st/docs/design/how-it-works/). One concurrent process does not prove that the host fixtures behave identically to libtest. The paired results must establish that.

Cargo-mutants 27.1.0 can classify nextest infrastructure errors as catches. This helper accepts test exit 100 as a test failure. It rejects empty-selection exit 4 and other non-test failures before comparing outcomes. The existing gate is unchanged. Keep this validation when proposing a router integration.

## Add nextest to the runner image

Nexus owns the image rebuild. The current LocalActions Dockerfile has no nextest installation. Add this fragment before `USER ubuntu` in `docker/ubuntu-act/Dockerfile` in the LocalActions repository:

```dockerfile
RUN set -eux; \
    curl -fLsS https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.143/cargo-nextest-0.9.143-x86_64-unknown-linux-gnu.tar.gz -o /var/tmp/nextest.tar.gz; \
    echo "66786b9abe23920d022a182d1416b1bbc8130dd4872a9553d76985a1708dcd1e  /var/tmp/nextest.tar.gz" | sha256sum -c -; \
    tar -xzf /var/tmp/nextest.tar.gz -C /usr/local/cargo/bin cargo-nextest; \
    chmod 755 /usr/local/cargo/bin/cargo-nextest; \
    /usr/local/cargo/bin/cargo-nextest nextest --version; \
    rm /var/tmp/nextest.tar.gz
```

This fragment targets the Nexus Linux x86_64 runner. The version and archive checksum were verified against the official release. See [nextest binary installation](https://nexte.st/docs/installation/pre-built-binaries/) for other platforms.

If nextest is unavailable, keep the existing Cargo gate. You may run the control with `--tools cargo`, but that does not complete the experiment. Do not silently substitute Cargo for the nextest column.
