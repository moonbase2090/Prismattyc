#!/usr/bin/env bash
# Assemble a local, unsigned Prismattyc.app bundle for macOS.
#
# This produces target/Prismattyc.app so the Dock and menu bar show
# "Prismattyc" instead of the "prismattyc-host" binary name.
# It is a local ad-hoc bundle. Release signing and notarization are
# scripts/release/package-macos.sh (see docs/release-process.md).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# 1. Build release binary.
cargo build -p prismattyc-host --release --locked

# 2. Ensure the .icns exists, generating it via the sibling script if absent.
ICNS="$ROOT/assets/brand/macos/prismattyc.icns"
if [[ ! -f "$ICNS" ]]; then
  echo "icns missing; generating via scripts/install-prismattyc-host-macos.sh"
  bash "$ROOT/scripts/install-prismattyc-host-macos.sh"
fi
if [[ ! -f "$ICNS" ]]; then
  echo "error: $ICNS still missing after running install-prismattyc-host-macos.sh" >&2
  exit 1
fi

# 3. Assemble the bundle.
APP="$ROOT/target/Prismattyc.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/crates/prismattyc-host/macos/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/target/release/prismattyc-host" "$APP/Contents/MacOS/prismattyc-host"
cp "$ICNS" "$APP/Contents/Resources/prismattyc.icns"

# 4. Ad-hoc sign for local Gatekeeper. Tolerated on failure (e.g. no
# codesign identity available); this is a local unsigned bundle either way.
if ! codesign --force --deep --sign - "$APP"; then
  echo "note: ad-hoc codesign failed; continuing with unsigned bundle" >&2
fi

# 5. Report.
echo "Built $APP (unsigned/ad-hoc). Release notarization is scripts/release/package-macos.sh."
