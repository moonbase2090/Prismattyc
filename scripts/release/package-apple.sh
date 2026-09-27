#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Assemble, sign, notarize, staple, and package the universal macOS app.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ $# -ne 5 ]]; then
  echo 'usage: package-apple.sh TAG VERSION BASE_VERSION BIN_ROOT DIST' >&2
  exit 2
fi
TAG="$1"
VERSION="$2"
BASE_VERSION="$3"
BIN_ROOT="$4"
DIST="$5"
if [[ "$(uname -s)" != Darwin ]]; then
  echo 'package-apple.sh must run on macOS' >&2
  exit 1
fi

for tool in codesign ditto file hdiutil lipo openssl python3 security shasum spctl xcrun; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "required tool not found: $tool" >&2
    exit 1
  }
done
[[ "$TAG" == "v$VERSION" ]] || {
  echo "tag $TAG does not match version $VERSION" >&2
  exit 1
}
PYTHONPATH="$ROOT/scripts/release" python3 - "$VERSION" "$BASE_VERSION" <<'PY'
import sys

from release_version import parse_release_version

version, base_version = parse_release_version(sys.argv[1])
if version != sys.argv[1] or base_version != sys.argv[2]:
    raise SystemExit("release version and base version do not match")
PY

WORK="$(mktemp -d /tmp/prismattyc-release.XXXXXX)"
KEYCHAIN="$WORK/release.keychain-db"
cleanup() {
  status=$?
  trap - EXIT
  security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  if ! rm -rf "$WORK"; then
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
KEYCHAIN_PASSWORD="$(openssl rand -hex 32)"

mkdir -p "$DIST"
APP="$WORK/Prismattyc.app"
MACOS="$APP/Contents/MacOS"
RESOURCES="$APP/Contents/Resources"
mkdir -p "$MACOS" "$RESOURCES"
cp "$ROOT/assets/brand/macos/prismattyc.icns" "$RESOURCES/prismattyc.icns"
cp "$ROOT/LICENSE" "$RESOURCES/MPL-2.0.txt"
cp "$ROOT/NOTICE.txt" "$RESOURCES/NOTICE.txt"
cp "$ROOT/crates/prismattyc-host/themes/OMARCHY-LICENSE.txt" "$RESOURCES/OMARCHY-LICENSE.txt"
cp "$ROOT/crates/prismattyc-host/assets/fonts/"*.txt "$RESOURCES/"
cp "$ROOT/scripts/install-prismattyc-terminfo.sh" "$MACOS/"
python3 "$ROOT/scripts/package-terminfo.py" --out "$RESOURCES/terminfo"

for name in prismattyc-host pmux pmuxd pmux-attach; do
  arm64="$BIN_ROOT/aarch64-apple-darwin/release/$name"
  x86_64="$BIN_ROOT/x86_64-apple-darwin/release/$name"
  [[ -x "$arm64" ]] || { echo "missing executable: $arm64" >&2; exit 1; }
  [[ -x "$x86_64" ]] || { echo "missing executable: $x86_64" >&2; exit 1; }
  PYTHONPATH="$ROOT/scripts/release" python3 - "$x86_64" "$VERSION" <<'PY'
import subprocess
import sys

from release_version import reports_release_version

binary, version = sys.argv[1:]
output = subprocess.run(
    [binary, "--version"], capture_output=True, text=True, timeout=10, check=True
).stdout
if not reports_release_version(output, version):
    raise SystemExit(f"{binary} reports a version that does not match {version}")
PY
  lipo -create "$arm64" "$x86_64" -output "$MACOS/$name"
  chmod 755 "$MACOS/$name"
  lipo -verify_arch arm64 x86_64 "$MACOS/$name"
done

cp "$ROOT/crates/prismattyc-host/macos/Info.plist" "$APP/Contents/Info.plist"
python3 - "$APP/Contents/Info.plist" "$BASE_VERSION" <<'PY'
import plistlib
import sys

path, version = sys.argv[1:]
with open(path, "rb") as stream:
    info = plistlib.load(stream)
info["CFBundleShortVersionString"] = version
info["CFBundleVersion"] = version
with open(path, "wb") as stream:
    plistlib.dump(info, stream, sort_keys=True)
PY

for name in APPLE_CERTIFICATE_P12 APPLE_CERTIFICATE_PASSWORD APPLE_NOTARY_ISSUER APPLE_NOTARY_KEY_ID APPLE_NOTARY_KEY; do
  if [[ -z "$(printenv "$name")" ]]; then
    echo "$name is required" >&2
    exit 1
  fi
done
CERTIFICATE="$WORK/developer-id.p12"
NOTARY_KEY="$WORK/notary-key.p8"
printf '%s' "$APPLE_CERTIFICATE_P12" | /usr/bin/base64 -D -o "$CERTIFICATE"
case "$APPLE_NOTARY_KEY" in
  *'-----BEGIN '*) printf '%s\n' "$APPLE_NOTARY_KEY" > "$NOTARY_KEY" ;;
  *) printf '%s' "$APPLE_NOTARY_KEY" | /usr/bin/base64 -D -o "$NOTARY_KEY" ;;
esac
grep -q -- '-----BEGIN PRIVATE KEY-----' "$NOTARY_KEY" || {
  echo 'APPLE_NOTARY_KEY must contain a PEM private key or base64-encoded PEM' >&2
  exit 1
}

security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security set-keychain-settings -lut 21600 "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security import "$CERTIFICATE" -f pkcs12 -k "$KEYCHAIN" \
  -P "$APPLE_CERTIFICATE_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple:,codesign: \
  -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
IDENTITY_LINES="$(security find-identity -v -p codesigning "$KEYCHAIN" \
  | sed -n '/"Developer ID Application:/p')"
[[ "$(printf '%s\n' "$IDENTITY_LINES" | sed '/^[[:space:]]*$/d' | wc -l | tr -d ' ')" == 1 ]] || {
  echo 'expected exactly one Developer ID Application identity in imported certificate' >&2
  exit 1
}
IDENTITY="$(printf '%s\n' "$IDENTITY_LINES" | awk '{print $2}')"
ENTITLEMENTS="$ROOT/scripts/release/prismattyc.entitlements"

sign_code() {
  codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" --sign "$IDENTITY" --keychain "$KEYCHAIN" "$1"
}
is_macho() {
  file -b "$1" | grep -q 'Mach-O'
}

echo 'Signing Prismattyc helper binaries'
for name in pmux pmuxd pmux-attach prismattyc-host; do
  binary="$MACOS/$name"
  is_macho "$binary" || { echo "not a Mach-O helper: $binary" >&2; exit 1; }
  sign_code "$binary"
done
while IFS= read -r -d '' binary; do
  if is_macho "$binary"; then
    sign_code "$binary"
  fi
done < <(find "$RESOURCES" "$APP/Contents/Frameworks" -type f -print0 2>/dev/null || true)

python3 - "$APP/Contents" "$WORK/nested-bundles.nul" <<'PY'
import os
from pathlib import Path
import sys

root, output = map(Path, sys.argv[1:])
suffixes = {".app", ".appex", ".bundle", ".framework", ".plugin", ".xpc"}
bundles = [
    path for path in root.rglob("*")
    if path.is_dir() and not path.is_symlink() and path.suffix in suffixes
]
bundles.sort(key=lambda path: (len(path.parts), str(path)), reverse=True)
with output.open("wb") as stream:
    for bundle in bundles:
        stream.write(os.fsencode(bundle) + b"\0")
PY
while IFS= read -r -d '' bundle; do
  sign_code "$bundle"
done < "$WORK/nested-bundles.nul"
sign_code "$APP"
codesign --verify --strict --verbose=2 "$APP"

ASSET="Prismattyc-$TAG-macos-universal"
SUBMIT_ZIP="$WORK/$ASSET-submit.zip"
DMG="$WORK/$ASSET.dmg"
ZIP="$WORK/$ASSET.zip"
ditto -c -k --keepParent "$APP" "$SUBMIT_ZIP"

submit_notarization() {
  artifact="$1"
  submission_json="$WORK/notary.json"
  submission_error="$WORK/notary.err"
  if xcrun notarytool submit "$artifact" --wait \
    --issuer "$APPLE_NOTARY_ISSUER" --key-id "$APPLE_NOTARY_KEY_ID" \
    --key "$NOTARY_KEY" --output-format json >"$submission_json" 2>"$submission_error"; then
    submission_status=0
  else
    submission_status=$?
  fi
  cat "$submission_error" >&2
  read -r id status < <(python3 - "$submission_json" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as stream:
        data = json.load(stream)
except (OSError, ValueError):
    data = {}
print(data.get("id", ""), data.get("status", ""))
PY
  )
  if [[ "$submission_status" -ne 0 || "$status" != Accepted ]]; then
    if [[ -n "$id" ]]; then
      echo "Notarization log for submission $id:" >&2
      xcrun notarytool log "$id" --issuer "$APPLE_NOTARY_ISSUER" \
        --key-id "$APPLE_NOTARY_KEY_ID" --key "$NOTARY_KEY" >&2 || true
    else
      cat "$submission_json" >&2 || true
    fi
    echo "notarization failed with status $status" >&2
    return 1
  fi
  echo "Notarization accepted for $(basename "$artifact") (submission $id)"
}

submit_notarization "$SUBMIT_ZIP"
xcrun stapler staple "$APP"
spctl -a -vvv -t exec "$APP"
xcrun stapler validate "$APP"
hdiutil create -volname Prismattyc -srcfolder "$APP" -ov -format UDZO "$DMG"
codesign --force --timestamp --sign "$IDENTITY" --keychain "$KEYCHAIN" "$DMG"
submit_notarization "$DMG"
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"
spctl -a -vvv -t exec "$APP"
codesign --verify --strict --verbose=2 "$APP"
codesign --verify --strict --verbose=2 "$DMG"
ditto -c -k --keepParent "$APP" "$ZIP"

for path in "$APP" "$DMG" "$ZIP"; do
  if [[ -e "$DIST/$(basename "$path")" ]]; then
    echo "refusing to overwrite existing deliverable: $DIST/$(basename "$path")" >&2
    exit 1
  fi
done
ditto "$APP" "$DIST/Prismattyc.app"
cp "$DMG" "$DIST/"
cp "$ZIP" "$DIST/"
(cd "$DIST" && shasum -a 256 "$(basename "$DMG")" "$(basename "$ZIP")" \
  > SHA256SUMS-macos)
echo "Created signed and notarized assets in $DIST"
