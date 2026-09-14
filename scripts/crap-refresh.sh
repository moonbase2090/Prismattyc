#!/usr/bin/env bash
# Refresh docs/crap-baseline.json from a live llvm-cov + cargo-crap run.
# Keep this file in git. Do not write it under build/ (gitignored).
# The committed baseline is the Local Actions runner's capture. A host
# run is not comparable (176 vs 181). Refuse unless this is the runner.
set -euo pipefail
if [[ "${GITHUB_ACTOR:-}" != "nektos/act" && "${CRAP_REFRESH_IN_RUNNER:-}" != "1" ]]; then
  echo "error: the CRAP baseline is the Local Actions runner's capture." >&2
  echo "Refresh with: local-actions run --event pull_request --job crap-refresh" >&2
  echo "A host capture is not comparable." >&2
  exit 1
fi
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
mkdir -p build
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$HOME/Projects/tools/bin:$PATH"

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
  echo "error: cargo-llvm-cov is missing (cargo install cargo-llvm-cov)" >&2
  exit 1
fi
if ! command -v cargo-crap >/dev/null 2>&1; then
  echo "error: cargo-crap is missing (cargo install cargo-crap, or ~/Projects/tools/bin)" >&2
  exit 1
fi

export PRISMATTYC_TEST_TIME_SCALE="${PRISMATTYC_TEST_TIME_SCALE:-4}"
rustup component add llvm-tools-preview
cargo llvm-cov --workspace --locked --lcov --output-path build/crap-lcov.info
cargo-crap --workspace \
  --lcov build/crap-lcov.info \
  --format json \
  --sort file \
  --output build/crap-raw.json

python3 - "$ROOT" <<'PY'
import json, sys
from pathlib import Path
root = Path(sys.argv[1]).resolve()
raw = json.loads((root / "build/crap-raw.json").read_text())
entries = []
for e in raw.get("entries", []):
    p = Path(e["file"])
    try:
        rel = str(p.resolve().relative_to(root))
    except ValueError:
        rel = e["file"]
    entries.append({
        "file": rel,
        "function": e["function"],
        "line": e["line"],
        "cyclomatic": e["cyclomatic"],
        "coverage": e.get("coverage"),
        "crap": e["crap"],
        "crate": e.get("crate"),
    })
entries.sort(key=lambda e: (e["file"], e["function"], e["line"]))
import importlib.util
spec = importlib.util.spec_from_file_location(
    "crap_gate", root / "scripts" / "crap-gate.py"
)
crap_gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(crap_gate)

old = {}
base_path = root / "docs/crap-baseline.json"
if base_path.exists():
    old = json.loads(base_path.read_text())
    old.setdefault("$schema", raw.get("$schema"))
    old.setdefault("version", raw.get("version"))
else:
    old = {"$schema": raw.get("$schema"), "version": raw.get("version")}
pkg = crap_gate.workspace_version(root / "Cargo.toml")
doc = crap_gate.stamp_baseline(old, entries, pkg, threshold=crap_gate.DEFAULT_THRESHOLD)
above = doc["above_count"]
dest = root / "docs/crap-baseline.json"
dest.write_text(json.dumps(doc, indent=2) + "\n")
print(f"wrote {dest} functions={len(entries)} above_{crap_gate.DEFAULT_THRESHOLD}={above}")
PY
