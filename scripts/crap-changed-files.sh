#!/usr/bin/env bash
# Print PR-touched files. Missing git metadata must not disable the CRAP gate.
# Run from the checkout root. Use its fetched origin/main as the comparison ref.
set -euo pipefail
unset GIT_DIR GIT_WORK_TREE

fail() {
  echo "error: cannot determine PR-touched files: $1" >&2
  echo "Use a checkout with origin/main fetched. In act, mount the host Docker socket" >&2
  echo "and keep the original worktree and its gitdir available to the Docker daemon." >&2
  exit 1
}

if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  git rev-parse --verify origin/main >/dev/null 2>&1 || fail "origin/main is missing."
  git diff --name-only origin/main...HEAD || fail "git could not compare origin/main with HEAD."
  exit 0
fi

# act copies worktrees without their host gitdir. Read the host checkout
# through Docker, as the mutants job does. Do not substitute an empty list.
if [[ ! -f .git ]]; then
  fail "no git metadata is available."
fi
gitdir="$(sed -n 's/^gitdir:[[:space:]]*//p' .git | tr -d '\r')"
case "$gitdir" in
  /*/.git/worktrees/*) main_git="${gitdir%%/.git/worktrees/*}/.git" ;;
  *) fail "unsupported worktree gitdir: $gitdir" ;;
esac
if docker info >/dev/null 2>&1; then
  dkr() { docker "$@"; }
elif sudo -n docker info >/dev/null 2>&1; then
  dkr() { sudo -n docker "$@"; }
else
  fail "Docker is unavailable to read the host worktree gitdir."
fi
dkr run --rm \
  -v "$main_git:$main_git:ro" \
  -v "$PWD:$PWD:ro" \
  -w "$PWD" \
  local-actions-runner:latest \
  git -c "safe.directory=$PWD" diff --name-only origin/main...HEAD \
  || fail "the read-only git sidecar could not compare origin/main with HEAD."
