#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Build, sign, notarize, and staple the Apple silicon Prismattyc.app zip.
#
# Run this on a Mac. It does not embed Apple ID passwords or API keys.
# Credentials stay in the notarytool keychain profile.
#
# Usage:
#   scripts/release/package-macos.sh --version 0.2.8 --out build/release-macos-arm64
#
# Environment:
#   PRISMATTYC_CODESIGN_IDENTITY
#     Default: Developer ID Application: Moonbase 2090 LLC (S24C53PD3Y)
#   PRISMATTYC_NOTARY_KEYCHAIN_PROFILE
#     Default: moonbase-notary
#   PRISMATTYC_NOTARY_TIMEOUT
#     Default: 6h. The first notarization for a team can take hours.
#     Examples: 30m, 2h, 3600.
#
# --bin-dir reuses arm64 release binaries instead of building them.
# --app notarizes an existing Prismattyc.app instead of assembling one.
# The shipped file is Prismattyc-vVERSION-macos-arm64.zip.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DEFAULT_IDENTITY="Developer ID Application: Moonbase 2090 LLC (S24C53PD3Y)"
DEFAULT_PROFILE="moonbase-notary"
IDENTITY="${PRISMATTYC_CODESIGN_IDENTITY:-$DEFAULT_IDENTITY}"
PROFILE="${PRISMATTYC_NOTARY_KEYCHAIN_PROFILE:-$DEFAULT_PROFILE}"
TIMEOUT="${PRISMATTYC_NOTARY_TIMEOUT:-6h}"
VERSION=""
OUT=""
BIN_DIR=""
APP_SRC=""
PRINT_NAME=0
WORK=""

usage() {
  cat <<'EOF'
Usage: scripts/release/package-macos.sh --version X.Y.Z --out DIRECTORY
       scripts/release/package-macos.sh --print-asset-name --version X.Y.Z

Options:
  --version X.Y.Z   Stable release version (0.2.0 or newer)
  --out DIRECTORY   New directory for the zip and its SHA256SUMS
  --bin-dir DIR     arm64 prismattyc-host, pmux, pmuxd, and pmux-attach
  --app PATH        Existing Prismattyc.app to sign and notarize
  --print-asset-name
                    Print Prismattyc-vX.Y.Z-macos-arm64.zip and exit

Environment:
  PRISMATTYC_CODESIGN_IDENTITY          Developer ID Application identity
  PRISMATTYC_NOTARY_KEYCHAIN_PROFILE    notarytool keychain profile (default moonbase-notary)
  PRISMATTYC_NOTARY_TIMEOUT             notarytool --wait timeout (default 6h)
EOF
}

log() {
  printf 'package-macos: %s\n' "$*"
}

on_exit() {
  status=$?
  trap - EXIT
  if [[ "$status" -ne 0 && -n "${WORK}" && -d "${WORK}" ]]; then
    printf 'package-macos: left work in %s\n' "$WORK" >&2
    if [[ -d "${WORK}/Prismattyc.app" && -n "${VERSION}" && -n "${OUT}" ]]; then
      printf 'package-macos: retry with: %s --version %s --out %s --app %s\n' \
        "$0" "$VERSION" "$OUT" "${WORK}/Prismattyc.app" >&2
    fi
  elif [[ -n "${WORK}" && -d "${WORK}" ]]; then
    rm -rf "$WORK"
  fi
  exit "$status"
}
trap on_exit EXIT

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

validate_version() {
  python3 - "$1" <<'PY'
import sys
version = sys.argv[1]
parts = version.split(".")
if len(parts) != 3 or any(not part.isdecimal() for part in parts):
    sys.exit(1)
if tuple(map(int, parts)) < (0, 2, 0):
    sys.exit(1)
PY
}

asset_name() {
  printf 'Prismattyc-v%s-macos-arm64.zip' "$VERSION"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      shift
      [[ $# -ge 1 && "$1" != --* ]] || fail "--version needs a value"
      VERSION="$1"
      shift
      ;;
    --out)
      shift
      [[ $# -ge 1 && "$1" != --* ]] || fail "--out needs a directory"
      OUT="$1"
      shift
      ;;
    --bin-dir)
      shift
      [[ $# -ge 1 && "$1" != --* ]] || fail "--bin-dir needs a directory"
      BIN_DIR="$1"
      shift
      ;;
    --app)
      shift
      [[ $# -ge 1 && "$1" != --* ]] || fail "--app needs a Prismattyc.app path"
      APP_SRC="$1"
      shift
      ;;
    --print-asset-name)
      PRINT_NAME=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'error: unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$VERSION" ]]; then
  usage >&2
  exit 2
fi
if ! validate_version "$VERSION"; then
  fail "use a stable version at or after 0.2.0"
fi
if [[ "$PRINT_NAME" -eq 1 ]]; then
  asset_name
  printf '\n'
  exit 0
fi
if [[ -z "$OUT" ]]; then
  usage >&2
  exit 2
fi
if [[ -n "$BIN_DIR" && -n "$APP_SRC" ]]; then
  fail "--bin-dir and --app cannot be combined"
fi
case "$IDENTITY$PROFILE" in
  *$'\n'*) fail "identity and keychain profile must be single-line values" ;;
esac
if [[ ! "$TIMEOUT" =~ ^[0-9]+[smh]?$ ]]; then
  fail "PRISMATTYC_NOTARY_TIMEOUT must look like 6h, 30m, or 3600"
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  fail "package-macos.sh must run on macOS. It signs and notarizes Prismattyc.app."
fi

need() {
  command -v "$1" >/dev/null 2>&1 || fail "required tool not found: $1"
}
need codesign
need ditto
need lipo
need python3
need spctl
need security
xcrun --find notarytool >/dev/null 2>&1 || fail "notarytool not found; install the Xcode command-line tools"
xcrun --find stapler >/dev/null 2>&1 || fail "stapler not found; install the Xcode command-line tools"

if ! security find-identity -v -p codesigning | grep -F "$IDENTITY" >/dev/null; then
  fail "codesign identity not found in the login keychain: $IDENTITY. Set PRISMATTYC_CODESIGN_IDENTITY to an installed Developer ID Application certificate."
fi
if ! xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null; then
  fail "notarytool keychain profile '$PROFILE' is not usable. Store it once with: xcrun notarytool store-credentials \"$PROFILE\" --apple-id APPLE_ID --team-id TEAM_ID"
fi

out_parent="$(dirname "$OUT")"
mkdir -p "$out_parent"
out_parent="$(cd "$out_parent" && pwd)"
OUT="$out_parent/$(basename "$OUT")"
if [[ -e "$OUT" ]]; then
  fail "output already exists: $OUT"
fi
if [[ -n "$BIN_DIR" ]]; then
  [[ -d "$BIN_DIR" ]] || fail "bin directory not found: $BIN_DIR"
  BIN_DIR="$(cd "$BIN_DIR" && pwd)"
fi
if [[ -n "$APP_SRC" ]]; then
  [[ -d "$APP_SRC" ]] || fail "app bundle not found: $APP_SRC"
  APP_SRC="$(cd "$APP_SRC" && pwd)"
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/prismattyc-macos.XXXXXX")"
APP="$WORK/Prismattyc.app"

if [[ -n "$APP_SRC" ]]; then
  log "copying $APP_SRC"
  ditto "$APP_SRC" "$APP"
else
  log "assembling Prismattyc.app"
  assemble=(bash "$ROOT/scripts/install-prismattyc-host-macos.sh" --release-bundle "$WORK")
  if [[ -n "$BIN_DIR" ]]; then
    assemble+=(--bin-dir "$BIN_DIR")
  fi
  PRISMATTYC_BUNDLE_VERSION="$VERSION" "${assemble[@]}"
fi
[[ -d "$APP/Contents/MacOS" ]] || fail "assembly did not produce $APP"

python3 - "$APP/Contents/Info.plist" "$VERSION" <<'PY'
import plistlib
import sys
path, version = sys.argv[1], sys.argv[2]
with open(path, "rb") as stream:
    info = plistlib.load(stream)
errors = []
if info.get("CFBundleShortVersionString") != version:
    errors.append(
        "CFBundleShortVersionString is %r, want %s"
        % (info.get("CFBundleShortVersionString"), version)
    )
if info.get("CFBundleExecutable") != "prismattyc-host":
    errors.append("CFBundleExecutable must be prismattyc-host")
if info.get("CFBundleIdentifier") != "dev.prismattyc.host":
    errors.append(
        "unexpected CFBundleIdentifier %r" % info.get("CFBundleIdentifier")
    )
if errors:
    sys.stderr.write("\n".join(errors) + "\n")
    sys.exit(1)
PY

for name in prismattyc-host pmux pmuxd pmux-attach; do
  bin="$APP/Contents/MacOS/$name"
  [[ -x "$bin" ]] || fail "missing executable $bin"
  arch="$(lipo -archs "$bin" | tr -d '[:space:]')"
  [[ "$arch" == "arm64" ]] || fail "$name architecture is '$arch'; only arm64 is packaged (no universal or Intel lipo)"
  python3 - "$bin" "$VERSION" <<'PY' || fail "$name does not report version $VERSION"
import subprocess, sys
binary, version = sys.argv[1], sys.argv[2]
try:
    result = subprocess.run(
        [binary, "--version"],
        capture_output=True,
        timeout=5,
        check=True,
    )
except Exception as error:
    sys.stderr.write("%s\n" % error)
    sys.exit(1)
if version not in result.stdout.decode().split():
    sys.stderr.write(result.stdout.decode())
    sys.exit(1)
PY
done

is_macho() {
  local magic
  magic="$(od -An -t x1 -N 4 "$1" | tr -d '[:space:]')"
  [[ "$magic" == "cffaedfe" ]]
}

log "signing nested binaries, then Prismattyc.app"
while IFS= read -r path; do
  [[ -n "$path" ]] || continue
  is_macho "$path" || continue
  codesign --force --options runtime --timestamp --sign "$IDENTITY" "$path" \
    || fail "codesign failed for $path"
  codesign -dv "$path" 2>&1 | grep -q 'flags=.*runtime' \
    || fail "hardened runtime was not set on $path"
done < <(python3 - "$APP" <<'PY'
import pathlib, sys
root = pathlib.Path(sys.argv[1])
paths = [path for path in root.rglob("*") if path.is_file()]
paths.sort(key=lambda path: len(path.parts), reverse=True)
for path in paths:
    print(path)
PY
)
codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP" \
  || fail "codesign failed for $APP"
signature="$(codesign -dv --verbose=4 "$APP" 2>&1 || true)"
printf '%s\n' "$signature" | grep -q 'flags=.*runtime' || fail "hardened runtime was not set on Prismattyc.app"
printf '%s\n' "$signature" | grep -q 'Timestamp=' || fail "codesign timestamp is missing"
printf '%s\n' "$signature" | grep -q 'Timestamp=none' && fail "codesign timestamp was not applied"
printf '%s\n' "$signature" | grep -F "$IDENTITY" >/dev/null || fail "signature identity does not match $IDENTITY"
codesign --verify --deep --strict --verbose=2 "$APP" || fail "codesign verify failed"

zip_app() {
  local app="$1"
  local archive="$2"
  rm -f "$archive"
  if command -v dot_clean >/dev/null 2>&1; then
    dot_clean -m "$app"
  fi
  ditto -c -k --keepParent "$app" "$archive"
  python3 "$ROOT/scripts/release/strip-appledouble.py" "$archive"
}

verify_zip() {
  local archive="$1"
  local stapled="$2"
  local dest="$WORK/verify-zip"
  rm -rf "$dest"
  mkdir -p "$dest"
  ditto -x -k "$archive" "$dest"
  local extracted="$dest/Prismattyc.app"
  [[ -d "$extracted" ]] || fail "zip is missing Prismattyc.app"
  codesign --verify --deep --strict --verbose=2 "$extracted" \
    || fail "extracted app failed codesign verify"
  if [[ "$stapled" -eq 1 ]]; then
    [[ -f "$extracted/Contents/CodeResources" ]] || fail "stapled ticket is missing from the zip"
    xcrun stapler validate "$extracted" || fail "extracted app failed stapler validate"
    spctl --assess --type execute --verbose=4 "$extracted" \
      || fail "Gatekeeper rejected the extracted app"
  fi
  rm -rf "$dest"
}

submit_zip="$WORK/Prismattyc-submit.zip"
log "creating notarization archive"
zip_app "$APP" "$submit_zip"
verify_zip "$submit_zip" 0

log "submitting for notarization (profile $PROFILE, timeout $TIMEOUT)"
submit_json="$WORK/notary.json"
set +e
xcrun notarytool submit "$submit_zip" \
  --keychain-profile "$PROFILE" \
  --wait \
  --timeout "$TIMEOUT" \
  --output-format json \
  >"$submit_json" 2>"$WORK/notary.err"
submit_status=$?
set -e
if [[ "$submit_status" -ne 0 ]]; then
  printf 'notarytool submit exited %s\n' "$submit_status" >&2
  cat "$WORK/notary.err" >&2 || true
  cat "$submit_json" >&2 || true
fi
notary_fields="$(python3 - "$submit_json" <<'PY'
import json, sys
path = sys.argv[1]
try:
    data = json.load(open(path))
except Exception as error:
    sys.stderr.write("unreadable notarytool output: %s\n" % error)
    sys.exit(2)
sys.stdout.write("%s\n%s\n" % (data.get("id") or "", data.get("status") or ""))
PY
)" || fail "could not read notarytool status"
notary_id="$(printf '%s\n' "$notary_fields" | sed -n '1p')"
notary_status="$(printf '%s\n' "$notary_fields" | sed -n '2p')"
if [[ "$notary_status" != "Accepted" ]]; then
  if [[ -n "$notary_id" ]]; then
    printf 'submission id: %s\n' "$notary_id" >&2
    xcrun notarytool log "$notary_id" --keychain-profile "$PROFILE" >&2 || true
  fi
  fail "notarization status: ${notary_status:-unknown}"
fi
log "notarization accepted ($notary_id)"

xcrun stapler staple "$APP" || fail "stapler failed"
xcrun stapler validate "$APP" || fail "stapler validate failed"
spctl --assess --type execute --verbose=4 "$APP" || fail "Gatekeeper rejected Prismattyc.app"
codesign --verify --deep --strict --verbose=2 "$APP" || fail "codesign verify failed after stapling"

release_zip="$WORK/$(asset_name)"
log "creating $(asset_name)"
zip_app "$APP" "$release_zip"
verify_zip "$release_zip" 1

mkdir "$OUT"
cp "$release_zip" "$OUT/$(asset_name)"
python3 "$ROOT/scripts/release/write-sha256sums.py" "$OUT"
log "wrote $OUT/$(asset_name)"
cat "$OUT/SHA256SUMS"
