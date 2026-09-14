# Acceptance pipeline

Use this pipeline to run Prismattyc acceptance features and acceptance
mutation. The pipeline uses the [Acceptance-Pipeline-Specification](https://github.com/unclebob/Acceptance-Pipeline-Specification) tools.

## Keep tools outside Prismattyc

Keep APS source and project-specific adapter source under
`~/Projects/tools`. Do not copy APS source into this repository.

Use this layout:

```text
~/Projects/tools/
├── Acceptance-Pipeline-Specification/
├── aps-termwright/
└── bin/
    ├── aps-termwright
    ├── gherkin-ir-dry-checker
    ├── gherkin-mutator
    └── gherkin-parser
```

The current APS source revision is `accaa33d503340c56513ef387258f8da929ba902`.
Build the Go fallback tools from that checkout:

```bash
cd "$HOME/Projects/tools/Acceptance-Pipeline-Specification"
go test ./...
go build -o "$HOME/Projects/tools/bin/gherkin-parser" ./cmd/gherkin-parser
go build -o "$HOME/Projects/tools/bin/gherkin-ir-dry-checker" ./cmd/gherkin-ir-dry-checker
go build -o "$HOME/Projects/tools/bin/gherkin-mutator" ./cmd/gherkin-mutator

cd "$HOME/Projects/tools/aps-termwright"
cargo build --release --locked
install -m 0755 target/release/aps-termwright \
  "$HOME/Projects/tools/bin/aps-termwright"
```

Set `APS_TOOLS_ROOT` when you use another tools directory.

## Run acceptance tests

Run all acceptance features:

```bash
./scripts/acceptance.sh
```

Run one feature:

```bash
./scripts/acceptance.sh classic-shell
```

The script parses each feature into JSON IR. It runs the IR dry checker. It
then generates deterministic Termwright YAML and runs the generated files.
Build files go under `build/acceptance/`.

The adapter sends typed input in chunks of at most 16 characters. It waits for
150 milliseconds after each chunk. This keeps the acceptance run reliable while
PT-176 is validated. Once the PT-176 key-burst proof passes, this workaround may
be relaxed; leave the adapter pacing in place until that 10/10 proof is recorded.

## Run acceptance mutation

Run the deliberate mutation check:

```bash
./scripts/acceptance-mutate.sh
```

The script copies each feature into `build/acceptance-mutation/` before the
mutator writes its stamp and manifest. It does not modify the tracked feature.
The first slice must report:

```text
total=5 killed=5 survived=0 errors=0
```

The five mutations cover the shell dimensions and marker, plus the color text
and foreground list. The generator uses fixed assertions. It uses example
values for input and session dimensions. A changed example therefore fails the
acceptance test.

Use `--level full` to run every mutation. Use `hard` or `soft` when you add a
project wrapper around the mutator and want differential reuse. The project
script uses `full` so each deliberate run gives a fresh result.

## cargo-mutants gate (PT-225)

This is a separate gate from Gherkin acceptance mutation. Keep
`scripts/acceptance-mutate.sh`. Do not replace it.

On the 32 GiB Nexus host, run `scripts/la-staged-pr.sh` instead of
one-shot `local-actions run --event pull_request`. Mutants is stage 2.
The script waits for each job to reach a terminal
`local-actions status` before it reclaims. A queued exit 0 is not a
pass. It then reclaims act leftovers, SSD scratch, and zram, and
checks host headroom.

The Local Actions job `mutants` is a merge gate. It installs
`cargo-mutants` 27.1.0 in the job. It runs `scripts/mutants-pr.sh`:

1. Write `git diff origin/<base>...HEAD` to `build/git.diff`.
   `--in-diff` needs a file. Do not use process substitution.
2. List workspace crates whose `src/` or `build.rs` changed.
3. Run `cargo mutants --no-shuffle -vV --in-diff build/git.diff -p <crate> -- -- --test-threads=1`.
   cargo-mutants forwards args after the first `--` to `cargo test`. The
   second `--` sends `--test-threads=1` to the harness. Serial tests
   avoid a parallel-load flake in the mux suite (PT-249). Scratch copies
   go under `${XDG_CACHE_HOME:-$HOME/.cache}/prismattyc/mutants` on the
   host. `/tmp` is tmpfs on this host. The gate refuses TMPDIR when the
   path is under `/tmp` or when `findmnt -no FSTYPE` reports `tmpfs`.
   The Local Actions `mutants` job bind-mounts host
   `/var/cache/prismattyc/mutants` at `/cache/prismattyc/mutants` and
   sets `MUTANTS_TMPDIR` and `TMPDIR` to that path. The scripts remove
   `cargo-mutants-*` there after the run.
4. Run `scripts/mutants-gate.py` on `mutants.out/outcomes.json`.

The gate fails when the caught rate is below 80%. Caught rate is
caught / (caught + missed + timeout). Unviable mutants do not count.
The script prints every missed mutant. An empty diff, or a diff with no
mutatable Rust, skips the run and passes. An empty `--in-diff` file is
valid for `cargo-mutants`. Missing git metadata fails the job. On Local
Actions the overlay copy has a worktree `.git` file whose gitdir is on
the host; `mutants-pr.sh` computes the diff in a docker sidecar that
bind-mounts that gitdir.

A nightly job runs `scripts/mutants-nightly.sh` on every crate and
archives `build/mutants/<crate>`. Dispatch:

```bash
local-actions run --event workflow_dispatch --job mutants-nightly
```

The nightly job is not the PR 80% gate. A failed unmutated baseline
still fails that crate.

```bash
python3 scripts/mutants-gate_test.py
./scripts/mutants-pr.sh
```

## Generator contract

Run the external generator with exactly two arguments:

```bash
aps-termwright acceptance-entrypoint-generator JSON_IR GENERATED_DIR
```

The generator writes one YAML file for each scenario and example row. It writes
metadata under `GENERATED_DIR/metadata/`. The metadata hash covers only the
generated YAML files.

The generator supports this step vocabulary:

| Gherkin step | Generated Termwright behavior |
| --- | --- |
| `Given a nested prismattyc shell of <cols> columns and <rows> rows` | Configure a nested shell session. |
| `When I type "<text>" and press Enter` | Type text and press Enter. |
| `When I press <key>` | Press one key. |
| `When I wait for the screen to settle` | Wait for an idle screen. |
| `Then the screen shows "<text>"` | Wait for and assert text. |
| `Then the screen does not show "<text>"` | Assert that text is absent. |
| `Then the terminal reports <cols> columns and <rows> rows` | Run `echo "cols=$(tput cols) rows=$(tput lines)"` and assert one unambiguous line. |

Unknown step text fails generation. The worker regenerates YAML from each
mutated JSON IR. It runs `termwright run-steps` for each generated scenario.
The generated YAML uses fixed assertions and example values for setup and
input. This is a deliberate deviation from the APS runtime-loading model. The
worker regenerates entry points for each mutation. The implementation hash
still covers generated files only.
## Worker protocol

Start the persistent worker with:

```bash
aps-termwright worker --prismattyc "$PWD/target/debug/prismattyc"
```

Send one JSON job per line. Receive one JSON response per line. The worker
writes protocol responses only to standard output. It writes diagnostics to
standard error. Outcomes are `test_success`, `test_failure`, and
`infrastructure_error`.

## CRAP gate (PT-226)

The Local Actions job `crap` is a merge gate. It runs
`cargo llvm-cov --workspace --lcov` then `cargo crap --workspace --lcov`
(`cargo-crap` 0.4.3). The gate fails when:

- the count of functions with CRAP above 30 grows versus
  [docs/crap-baseline.json](crap-baseline.json)
- a new function in a file the PR touches scores above 30
- a function in a file the PR touches crosses from 30 or below to above 30

Existing functions already above 30 may stay. `build/` stays gitignored.
The committed baseline is `docs/crap-baseline.json`. That file stores
`above_count` (functions above 30) and one JSON entry per function.

`cargo-crap` lives in `~/Projects/tools/bin` or `cargo install cargo-crap`.
Do not copy its source into this repository.

The baseline is the runner's capture. Refresh with
`local-actions run --event pull_request --job crap-refresh`. A host
capture is not comparable. `scripts/crap-refresh.sh` refuses unless it
runs inside the Local Actions runner (`GITHUB_ACTOR=nektos/act` or
`CRAP_REFRESH_IN_RUNNER=1`). The job writes
`docs/crap-baseline.json` in the worktree. Commit that file.

A markdown report for local review still goes under `build/`:

```bash
cargo llvm-cov --workspace --lcov --output-path build/crap-lcov.info
cargo-crap --workspace \
  --lcov build/crap-lcov.info \
  --format markdown \
  --output build/crap-report.md
```