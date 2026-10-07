#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Native performance acceptance for the pixel-alpha conversion path.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

cargo run --release --locked -p prismattyc-host --example premultiply_bench -- --acceptance
