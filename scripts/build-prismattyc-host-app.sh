#!/usr/bin/env bash
# Assemble a local, unsigned Prismattyc.app bundle for macOS.
#
# Signing/notarization are out of scope here (deferred; see docs/macos.md).
# This produces target/Prismattyc.app so the Dock and menu bar show
# "Prismattyc" instead of the "prismattyc-host" binary name.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Build all app-owned executables against one verified bundle.
export PRISMATTYC_PLUGIN_BUNDLE_DIR="$(python3 scripts/release/plugin_bundle_provenance.py prepare)"
export CARGO_ENCODED_RUSTFLAGS="$(python3 "$ROOT/scripts/release/plugin_bundle_provenance.py" flags)"
BUILD_TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
export CARGO_TARGET_DIR="$BUILD_TARGET"
cargo +1.90.0 build --workspace --bins --release --locked
cargo +1.90.0 build --manifest-path tools/activity-monitor/Cargo.toml --target-dir "$BUILD_TARGET" --release --locked

# 2. Ensure the .icns exists, generating it via the sibling script if absent.
ICNS="$ROOT/assets/brand/macos/prismattyc.icns"
if [[ ! -f "$ICNS" ]]; then
  echo "icns missing; generating via scripts/install-prismattyc-host-macos.sh"
  bash "$ROOT/scripts/install-prismattyc-host-macos.sh" --icns-only "$ICNS"
fi
if [[ ! -f "$ICNS" ]]; then
  echo "error: $ICNS still missing after running install-prismattyc-host-macos.sh" >&2
  exit 1
fi

# Verify physical build outputs before replacing the previous local app.
python3 "$ROOT/scripts/release/plugin_bundle_provenance.py" verify --bins "$BUILD_TARGET/release" --out "$BUILD_TARGET/plugin-bundle.json"

# 3. Assemble the bundle.
APP="${APP_BUILD_DEST:-$BUILD_TARGET/Prismattyc.app}"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/crates/prismattyc-host/macos/Info.plist" "$APP/Contents/Info.plist"
for name in prismattyc-host pmux pmuxd pmux-attach pmux-mcp prismattyc pmux-activity-monitor; do
  cp "$BUILD_TARGET/release/$name" "$APP/Contents/MacOS/$name"
done
cp "$BUILD_TARGET/plugin-bundle.json" "$APP/Contents/Resources/plugin-bundle.json"
cp "$ROOT/LICENSE" "$APP/Contents/Resources/MIT.txt"
cp "$ROOT/NOTICE.txt" "$APP/Contents/Resources/NOTICE.txt"
cp "$ROOT/crates/prismattyc-host/assets/fonts/"*.txt "$APP/Contents/Resources/"
cp "$ROOT/crates/prismattyc-host/themes/OMARCHY-LICENSE.txt" "$APP/Contents/Resources/OMARCHY-LICENSE.txt"
cp "$ICNS" "$APP/Contents/Resources/prismattyc.icns"
python3 "$ROOT/scripts/package-terminfo.py" --out "$APP/Contents/Resources/terminfo"
cp "$ROOT/scripts/install-prismattyc-terminfo.sh" "$APP/Contents/Resources/install-prismattyc-terminfo.sh"

python3 - "$APP" <<'PLIST'
import json, plistlib, sys
from pathlib import Path
app = Path(sys.argv[1])
provenance = json.loads((app / 'Contents/Resources/plugin-bundle.json').read_bytes())
path = app / 'Contents/Info.plist'
info = plistlib.loads(path.read_bytes())
version = provenance['identity']['core_version']
info.update(CFBundleShortVersionString=version, CFBundleVersion=version)
path.write_bytes(plistlib.dumps(info))
PLIST

# 4. Ad-hoc sign for local Gatekeeper. Tolerated on failure (e.g. no
# codesign identity available); this is a local unsigned bundle either way.
if ! codesign --force --deep --sign - "$APP"; then
  echo "note: ad-hoc codesign failed; continuing with unsigned bundle" >&2
fi

# 5. Report.
echo "Built $APP (unsigned/ad-hoc). Notarization deferred (docs/macos.md)."
