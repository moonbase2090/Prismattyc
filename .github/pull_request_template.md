## What changed

<!-- One paragraph. Ticket: PT-NNN. Workspace version bump: 0.1.NNN -->

## How it was verified

- [ ] Unit: `cargo test -p <crate>`; `clippy -D warnings`
- [ ] E2e step: `demo/spaces-e2e.sh` or sibling step name(s), or "no behaviour or performance claim"
- [ ] No-op test: what the step fails on if you revert this change to a no-op. If the answer is nothing, the step is wrong.
- [ ] Box run: driven in the demo box; screenshots attached below, or "no pixels changed"
- [ ] Idle: host-state asserts run after 2 s with no input
- [ ] No helper steps around the action under test

## Gates

- CRAP merge check: <!-- no new above-40 function or <=40 → >40 crossing in any PR-touched file -->
- CRAP report (informational): <!-- baseline → current above-40 count; previous-release count; -10 release target; top 10 per crate -->
<!-- Global count growth does not block merge. The separate crap-release job enforces the reduction target before a release tag, not per-merge version stamps. -->
- Mutants: <!-- caught / total on touched crates -->

## Seams touched

<!-- real child process · event-loop timing · subprocess race · paint path · none -->

## Screenshots

<!-- before / after -->
