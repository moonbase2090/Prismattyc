# Validate the polish follow-up

PR #375 merged with an explicit operator exception for two failed checks.
This follow-up fixes the scrollback test and adds missing pane-identity
coverage. It changes tests and documentation. It does not change terminal
behavior. The package version is 0.1.320.

## Scrollback check

The previous test searched the last 800 raw output bytes for `L100`.
A status-bar repaint can fill that suffix with ANSI sequences after the
terminal paints the live content. The assertion can then fail even when
the terminal has returned to the live tail.

The test now feeds the output into an emulator. It checks the visible
screen after Page Up, Home, and `q`. It also sends new input and checks
that the live output appears without the scrollback indicator. Its wait
uses the shared test-time budget.

## Exact-pane identity check

The new test starts a private daemon with two sessions and a split pane.
It opens the secondary pane by session ID, pane ID, and child PID. It
checks the returned identity and geometry. It rejects mismatched IDs,
cross-session targets, and stale child PIDs.

The test catches the four original mutations that weakened these identity
checks. The production lookup code remains unchanged.

## Results

| Check | Result |
| --- | --- |
| Host suite | 742 unit tests and 8 version tests passed |
| Mux suite | All 759 tests passed, including 23 interactive-attach tests |
| Scrollback repetition | Four additional focused runs passed |
| Clippy | Host and mux, all targets, warnings denied: passed |
| Exact-pane mutations | Four tested; all caught; unmutated baseline passed |
| Incremental host mutation score | 67 caught, 43 missed, 13 unviable; 60.9% of 110 scored |
| Formatting and package version | Passed |

The incremental host result preserves all 123 original identities. Four
previous misses now have caught results. The other 119 results carry
forward from the original run. This is not a new full-universe run. The
original 57.3% failure remains in its original report.

Local receipts are in `build/targeted/`. The file
`host-incremental-mutations.json` records the original report, follow-up
commit, unchanged production-prefix hash, and four updated identities.

The previous mux run stopped at its baseline. Its 161 mutations remain
separate work. A passing baseline does not establish their caught rate.

## Reproduce the targeted checks

Build the daemon and CLI before the private-daemon test:

```bash
cargo build --locked -p prismattyc-mux --bins
cargo test --locked -p prismattyc-host \
  exact_pane_checks_session_pane_and_child_identity -- --test-threads=1
cargo test --locked -p prismattyc-mux --test interactive_attach \
  -- --test-threads=1
```

For the four original host misses, use a disk-backed temporary directory.
Set an absolute target directory so the copied mutation source can use
the previously built daemon and CLI. Hold the repository heavy-job lock.

```bash
export CARGO_TARGET_DIR="$PWD/target"
export TMPDIR="$HOME/.cache/prismattyc/targeted-mutants"
mkdir -p "$TMPDIR"
cargo mutants --no-config --jobs 1 --no-shuffle \
  --file crates/prismattyc-host/src/attach_log.rs \
  -p prismattyc-host \
  --re 'replace (== with !=|&& with \|\|) in LogConnection::exact_pane' \
  --output build/targeted/exact-pane-mutants \
  -- --locked -- exact_pane_checks_session_pane_and_child_identity \
  --test-threads=1
```

The file filter is required. Cargo-mutants 27.1.0 does not apply the regex
filter to every struct-field mutation. Verify that discovery selects the
four intended identities before scoring the run.
