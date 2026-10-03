# Mutation testing

The Mutants nightly uses a pinned source commit for a complete workspace
rotation. Each scheduled run executes the next batch of 16 round-robin shards.
A second job runs cargo-mutants with `--in-diff` on current main, comparing the
current head with the pinned cycle commit. Changes therefore receive mutation
coverage while the full rotation progresses.

The planner runs `cargo mutants --list --json --workspace` in CI and records
counts by package. It estimates shard count from measured build and test phase
times in completed outcomes, aims for a four-hour shard with 25% headroom, and
caps the matrix at GitHub's 256-job limit. The first cycle bootstraps from
archived CI timings: `prismattyc-host` averaged 6.5 seconds to build a mutant
and 99.0 seconds to test it; `prismattyc-mux` averaged 7.6 and 70.7 seconds.
Other packages start at 8 seconds to build and 5 seconds to test. Each completed
batch updates these estimates for the next cycle.

The mutation run uses cargo-mutants 27.1.0 with `--in-place`,
`--baseline=skip`, `--sharding round-robin`, and Nextest. The full workspace
baseline runs first. Each mutant runs tests from its own package;
`--test-workspace=false` makes that scope explicit. The fast `mutants` profile
skips private-window, tmux, and walkthrough test cases and uses a 300-second
mutant timeout. The `mutants-slow` profile runs the full test set with longer
per-test limits and a 1,200-second mutant timeout.

Each mutant step stops after 300 minutes inside a 360-minute job, leaving time
to upload outcomes and release the heavy-job lock. The run script also sweeps
orphaned test services on normal exit. The workflow runs up to 16 matrix jobs
at once. GitHub documents a 6-hour job limit, a 256-job
matrix limit, and 20 concurrent standard jobs on the Free plan in its
[Actions limits](https://docs.github.com/en/actions/reference/limits).

## Jev shadow pilot

The optional Jev job runs beside the nightly mutation workflow after its selected
shard jobs finish. It reads the pinned cycle's mutant list and only evaluates
mutants with outcomes in that run. Jev predictions do not select tests, skip
mutants, or change mutation execution.

The job sends Jev requests only when the repository variable
**JEV_SHADOW_ENABLED** is **true** and both required secrets are available:

| Secret | Required | Purpose |
| --- | --- | --- |
| **CLOUDFLARE_ACCOUNT_ID** | Yes | Cloudflare account for the Jev request |
| **CLOUDFLARE_API_TOKEN** | Yes | Bearer token for Cloudflare's account API |
| **CLOUDFLARE_AI_GATEWAY_ID** | No | Adds the cf-aig-gateway-id header to select a specific gateway |

Use a token with **Workers AI Read**. Cloudflare's [run endpoint
reference](https://developers.cloudflare.com/api/resources/ai/methods/run/)
also accepts Workers AI Write. The job calls Cloudflare's
`POST /accounts/{account_id}/ai/run` endpoint with model
`typesafe/jev` and the structured input shown in the [Cloudflare Jev
model catalog](https://developers.cloudflare.com/ai/models/typesafe/jev/).
The workflow uses Cloudflare account authentication and has no TypeSafe key,
Custom Provider, or BYOK secret. Configure the selected AI Gateway for Unified
Billing and load prepaid credits as described in [Cloudflare's Unified Billing
docs](https://developers.cloudflare.com/ai-gateway/features/unified-billing/).

Each prediction request sends one mutant diff and its enclosing function
context. Every `MissedMutant` gets a second request with the observed
miss outcome. Jev classifies it as likely equivalent or a real test gap.
Requests run at one per second. Jev's [model
documentation](https://docs.typesafe.ai/models) currently lists 80 requests
per second and 100,000 tokens per second, and says those limits can change.
The client honors `Retry-After` and retries HTTP 429 and 529 with exponential
backoff. The job records usage and estimates input-token cost at the price in
the Cloudflare catalog.

Without the opt-in variable or either required secret, the job makes no network
request and uploads a dry-run report with the matched mutant count and planned
request counts. The script defaults to dry-run mode:

~~~bash
python3 scripts/jev-shadow.py \
  --plan build/mutants/plan.json \
  --mutants build/mutants/mutants-list.json \
  --outcomes build/mutant-shards \
  --repo cycle-source \
  --output build/jev-shadow/predictions.json \
  --summary build/jev-shadow/summary.md
~~~

The live report measures package-pick recall on caught mutants and missed-score
calibration against shard outcomes. It classifies every missed mutant.
Triage precision uses the first 25 missed mutants as a review sample. It stays
unavailable until maintainers label sampled stable IDs in
`scripts/jev-shadow-labels.json` as likely-equivalent or real-test-gap.
Human labels determine triage precision. A test survivor alone cannot show
whether a mutation is equivalent or exposes a test gap.

Rotation state is saved only after every selected shard completes. If a runner
stops early or an outcome file is missing, the report marks the batch partial
and the next schedule retries the same shard IDs. The report includes the
number tested, caught, missed, timed out, and unviable, plus build and test
seconds per package. Download the `mutants-nightly-missed` artifact to inspect
the report; download `mutants-nightly-shard-N` to inspect an individual
`outcomes.json` and its text summaries.

## Run a batch in GitHub Actions

Dispatch `Mutants nightly` on a branch to validate its first 16 fast-profile
shards:

```bash
gh workflow run mutants-nightly.yml \
  --repo moonbase2090/Prismattyc \
  --ref YOUR_BRANCH \
  -f nextest_profile=mutants \
  -f start_shard=0
```

To mutation-test branch changes with the full test profile, select
`mutants-slow`. This runs only the branch's Rust diff against `origin/main`; the
full-workspace rotation matrix is skipped. The job runs a full Nextest baseline,
then mutates only that diff. Its mutant timeout is 1,200 seconds.
For fast-profile batches, dispatch the same commit with `start_shard` set to the
next index shown in the plan artifact. Manual dispatches do not update the
scheduled rotation state.

## Download the nightly report

Find the latest run and note its run ID:

```bash
gh run list --repo moonbase2090/Prismattyc \
  --workflow mutants-nightly.yml --limit 5
```

Download its missed-mutant report:

```bash
gh run download RUN_ID --repo moonbase2090/Prismattyc \
  --name mutants-nightly-missed \
  --dir build/mutants-nightly
```

Read `build/mutants-nightly/missed-mutants.txt`. The GitHub Actions summary
shows the batch plan, per-package measurements, and any incomplete shard IDs.
