# Mutation testing

The nightly workflow runs the full workspace across eight GitHub-hosted shards. It merges their outcomes and uploads one artifact named `mutants-nightly-missed` with the missed-mutant list.

## Download the nightly report

Find the latest run and note its run ID:

```bash
gh run list --repo moonbase2090/Prismattyc --workflow mutants-nightly.yml --limit 5
```

Download its missed-mutant report:

```bash
gh run download RUN_ID --repo moonbase2090/Prismattyc \
  --name mutants-nightly-missed \
  --dir build/mutants-nightly
```

Read `build/mutants-nightly/missed-mutants.txt`. Each `MISSED` line names a mutant that the tests did not catch. The report also shows shard completeness and outcome counts.

## Run mutation tests on a change

Run a local mutation pass only when a change needs that feedback. Create a diff from the base branch, then run cargo-mutants at low priority with one job:

```bash
mkdir -p build
git fetch origin main
git diff origin/main...HEAD > build/mutation.diff
nice -n 19 cargo mutants --jobs 1 --no-shuffle \
  --in-diff build/mutation.diff -- -- --test-threads=1
```

Review `mutants.out/missed.txt` and `mutants.out/outcomes.json` for the result.
