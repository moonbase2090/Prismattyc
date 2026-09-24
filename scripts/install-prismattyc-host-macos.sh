#!/usr/bin/env bash
# Build prismattyc.icns and assemble Prismattyc.app for macOS.
#
# The raw `cargo install` binary shows a per-session Dock tile via a runtime
# PNG, but that image dies with the process, so the Dock's quit animation
# falls back to the generic executable icon. A real .app bundle carries a
# persistent icon in Finder, the Dock, and through quit.
#
# Usage:
#   scripts/install-prismattyc-host-macos.sh            # icns + Prismattyc.app -> ~/Applications
#   scripts/install-prismattyc-host-macos.sh --icns-only [OUT.icns]
#   APP_DEST=/path scripts/install-prismattyc-host-macos.sh   # install app elsewhere
#   scripts/install-prismattyc-host-macos.sh --release-bundle DIR [--bin-dir DIR]
#
# Always writes APP_DEST/Prismattyc.app (default ~/Applications). Also
# replaces the embedded binary in any other existing Prismattyc.app that we
# can find (/Applications, $ROOT/target, Spotlight), so a Dock-pinned copy
# cannot stay stale after `prismattyc update`.
#
# --release-bundle writes one unsigned app and does not ad-hoc sign it or
# refresh other copies. scripts/release/package-macos.sh uses that mode, then
# Developer ID signs and notarizes. Set PRISMATTYC_BUNDLE_VERSION to the
# release version so Info.plist matches the binaries. --bin-dir supplies
# prismattyc-host, pmux, pmuxd, and pmux-attach; otherwise the release bundle
# is built from this checkout, not from ~/.cargo/bin.
#
# The bundle is Prismattyc.app (display name "Prismattyc"); the executable
# inside is prismattyc-host. Icon art is the Continuous beam mark.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PNG="$ROOT/assets/brand/png"
ICNS="$ROOT/assets/brand/macos/prismattyc.icns"
APP_NAME="Prismattyc"
BUNDLE_ID="dev.prismattyc.host"
APP_DEST="${APP_DEST:-$HOME/Applications}"
SRC="$PNG/prismattyc-tile-1024.png"

ICNS_ONLY=0
RELEASE_BUNDLE=0
BIN_DIR=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --icns-only)
      ICNS_ONLY=1
      shift
      if [[ $# -gt 0 && "$1" != --* ]]; then
        ICNS="$1"
        shift
      fi
      ;;
    --release-bundle)
      RELEASE_BUNDLE=1
      shift
      if [[ $# -lt 1 || "$1" == --* ]]; then
        echo "error: --release-bundle needs a directory" >&2
        exit 2
      fi
      APP_DEST="$1"
      shift
      ;;
    --bin-dir)
      shift
      if [[ $# -lt 1 || "$1" == --* ]]; then
        echo "error: --bin-dir needs a directory" >&2
        exit 2
      fi
      BIN_DIR="$1"
      shift
      ;;
    -h|--help)
      echo "Usage: scripts/install-prismattyc-host-macos.sh [--icns-only [OUT.icns]]"
      echo "       scripts/install-prismattyc-host-macos.sh --release-bundle DIR [--bin-dir DIR]"
      echo "APP_DEST overrides the dogfood install directory (default ~/Applications)."
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done
if [[ "$ICNS_ONLY" -eq 1 && "$RELEASE_BUNDLE" -eq 1 ]]; then
  echo "error: --icns-only and --release-bundle cannot be combined" >&2
  exit 2
fi
if [[ "$RELEASE_BUNDLE" -eq 0 && -n "$BIN_DIR" ]]; then
  echo "error: --bin-dir is only valid with --release-bundle" >&2
  exit 2
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/prism-iconset.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

build_icns() {
  if [[ ! -f "$SRC" ]]; then
    echo "error: missing $SRC" >&2
    return 1
  fi
  if ! command -v iconutil >/dev/null 2>&1; then
    echo "error: iconutil not found (need Xcode command-line tools)" >&2
    return 1
  fi
  mkdir -p "$(dirname "$ICNS")"
  local iconset="$WORK/prismattyc.iconset"
  mkdir -p "$iconset"
  # <pixels> <dest-name> : resize the dark master with sips
  make_icon() {
    sips -z "$1" "$1" "$SRC" --out "$iconset/$2" >/dev/null
  }
  make_icon 16   icon_16x16.png
  make_icon 32   icon_16x16@2x.png
  make_icon 32   icon_32x32.png
  make_icon 64   icon_32x32@2x.png
  make_icon 128  icon_128x128.png
  make_icon 256  icon_128x128@2x.png
  make_icon 256  icon_256x256.png
  make_icon 512  icon_256x256@2x.png
  make_icon 512  icon_512x512.png
  make_icon 1024 icon_512x512@2x.png
  iconutil -c icns -o "$ICNS" "$iconset"
  echo "icns -> $ICNS"
}

if [[ "$ICNS_ONLY" -eq 1 ]]; then
  if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "iconutil not available (need Darwin). SVG master is ready at:" >&2
    echo "  $ROOT/assets/brand/macos/prismattyc.svg" >&2
    echo "On a Mac, re-run this script to write $ICNS" >&2
    exit 0
  fi
  build_icns
  exit 0
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: Prismattyc.app install is macOS-only" >&2
  exit 1
fi

# Prefer a committed icns when iconutil is missing so `prismattyc update` still
# copies the host binary. Do not exit 0 here — that used to skip the app.
if [[ -f "$ICNS" ]]; then
  if command -v iconutil >/dev/null 2>&1; then
    build_icns
  else
    echo "iconutil missing; reusing $ICNS" >&2
  fi
else
  build_icns
fi
if [[ ! -f "$ICNS" ]]; then
  echo "error: $ICNS missing; cannot assemble Prismattyc.app" >&2
  exit 1
fi

# ---- Binary: cargo-installed copy if present, else a release build ---------
# A release bundle must come from this checkout or from --bin-dir. A
# ~/.cargo/bin copy can be a different version than the tag being signed.
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin/prismattyc-host"
if [[ "$RELEASE_BUNDLE" -eq 1 && -n "$BIN_DIR" ]]; then
  BIN="$BIN_DIR/prismattyc-host"
  echo "using $BIN"
elif [[ "$RELEASE_BUNDLE" -eq 1 ]]; then
  echo "building prismattyc-host (release)..."
  ( cd "$ROOT" && cargo build --release --locked -p prismattyc-host )
  BIN="$ROOT/target/release/prismattyc-host"
elif [[ -x "$CARGO_BIN" ]]; then
  BIN="$CARGO_BIN"
  echo "using $BIN"
else
  echo "building prismattyc-host (release)..."
  ( cd "$ROOT" && cargo build --release --locked -p prismattyc-host )
  BIN="$ROOT/target/release/prismattyc-host"
fi
if [[ ! -x "$BIN" ]]; then
  echo "error: host binary not found at $BIN" >&2
  exit 1
fi
if [[ -n "${PRISMATTYC_BUNDLE_VERSION:-}" ]]; then
  VERSION="$PRISMATTYC_BUNDLE_VERSION"
else
  VERSION="$(cd "$ROOT" && cargo metadata --no-deps --format-version 1 \
    | /usr/bin/python3 -c 'import sys,json; pkgs=json.load(sys.stdin)["packages"]; print(next(p["version"] for p in pkgs if p["name"]=="prismattyc-host"))' \
    2>/dev/null || echo "0.0.0")"
  if [[ "$RELEASE_BUNDLE" -eq 1 && "$VERSION" == "0.0.0" ]]; then
    echo "error: could not read the workspace version for the release bundle" >&2
    exit 1
  fi
fi

sign_bundle() {
  local dest="$1"
  if [[ "$RELEASE_BUNDLE" -eq 1 ]]; then
    return 0
  fi
  if command -v codesign >/dev/null 2>&1; then
    # Sign nested mux executables too. Finder-launched apps can be denied when
    # a nested helper retains only its linker signature under a newly signed
    # outer bundle.
    codesign --force --deep --sign - --identifier "$BUNDLE_ID" "$dest" \
      && echo "signed (ad-hoc, deep) -> $dest" \
      || echo "warning: codesign failed; the app still runs unsigned" >&2
  fi
  touch "$dest"
  local lsregister="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
  if [[ -x "$lsregister" ]]; then
    "$lsregister" -f "$dest" || true
  fi
}

# Replace every embedded executable via temp+mv so a running app does not
# hit ETXTBSY. Copy from the newly assembled destination bundle so alternate
# Dock-pinned copies get the same self-contained command set.
refresh_bundle_binaries() {
  local dest="$1"
  [[ -d "$dest/Contents/MacOS" ]] || return 0
  [[ -f "$dest/Contents/MacOS/prismattyc-host" ]] || return 0
  local name source_bin tmp
  for name in prismattyc-host pmux pmuxd pmux-attach; do
    source_bin="$DEST_APP/Contents/MacOS/$name"
    [[ -x "$source_bin" ]] || {
      echo "warning: cannot refresh $dest; missing $source_bin" >&2
      return 1
    }
    tmp="$(mktemp "$dest/Contents/MacOS/.$name.XXXXXX")"
    cp "$source_bin" "$tmp"
    chmod +x "$tmp"
    mv -f "$tmp" "$dest/Contents/MacOS/$name"
  done
  mkdir -p "$dest/Contents/Resources"
  for notice in "$DEST_APP/Contents/Resources/"*.txt; do
    cp "$notice" "$dest/Contents/Resources/"
  done
  sign_bundle "$dest"
  echo "refreshed binaries -> $dest"
}

# ---- Assemble Prismattyc.app into APP_DEST ---------------------------------
if [[ "$RELEASE_BUNDLE" -eq 1 ]]; then
  export COPYFILE_DISABLE=1
fi
APP="$WORK/$APP_NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/prismattyc-host"
cp "$ICNS" "$APP/Contents/Resources/prismattyc.icns"
cp "$ROOT/LICENSE" "$APP/Contents/Resources/MPL-2.0.txt"
cp "$ROOT/NOTICE.txt" "$APP/Contents/Resources/NOTICE.txt"
cp "$ROOT/crates/prismattyc-host/assets/fonts/"*.txt "$APP/Contents/Resources/"
cp "$ROOT/crates/prismattyc-host/themes/OMARCHY-LICENSE.txt" "$APP/Contents/Resources/OMARCHY-LICENSE.txt"

# Finder and Dock launches do not provide the shell PATH. Build and bundle
# the mux front door and its helper binaries from this checkout so host-only
# updates cannot package stale executables from Cargo bin or ambient PATH.
if [[ "$RELEASE_BUNDLE" -eq 0 || -z "$BIN_DIR" ]]; then
  echo "building prismattyc-mux helpers (release)..."
  ( cd "$ROOT" && CARGO_TARGET_DIR="$ROOT/target" cargo build --release --locked -p prismattyc-mux --bins )
fi
for name in pmux pmuxd pmux-attach; do
  if [[ "$RELEASE_BUNDLE" -eq 1 && -n "$BIN_DIR" ]]; then
    source_bin="$BIN_DIR/$name"
  else
    source_bin="$ROOT/target/release/$name"
  fi
  [[ -x "$source_bin" ]] || {
    echo "error: mux helper build did not produce $source_bin" >&2
    exit 1
  }
  cp "$source_bin" "$APP/Contents/MacOS/$name"
  chmod +x "$APP/Contents/MacOS/$name"
done

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>$APP_NAME</string>
	<key>CFBundleDisplayName</key>
	<string>$APP_NAME</string>
	<key>CFBundleIdentifier</key>
	<string>$BUNDLE_ID</string>
	<key>CFBundleExecutable</key>
	<string>prismattyc-host</string>
	<key>CFBundleIconFile</key>
	<string>prismattyc</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
PLIST

mkdir -p "$APP_DEST"
DEST_APP="$APP_DEST/$APP_NAME.app"
rm -rf "$DEST_APP"
cp -R "$APP" "$DEST_APP"
sign_bundle "$DEST_APP"
if [[ "$RELEASE_BUNDLE" -eq 1 ]]; then
  echo "release bundle -> $DEST_APP"
  exit 0
fi
echo "app  -> $DEST_APP"

# Refresh every other copy we can find. Dock often keeps /Applications or
# target/Prismattyc.app, which `prismattyc update` used to leave stale.
same_dest() {
  local a b
  a="$(cd "$1" && pwd)"
  b="$(cd "$2" && pwd)"
  [[ "$a" == "$b" ]]
}

refresh_if_other() {
  local dest="$1"
  [[ -d "$dest/Contents/MacOS" ]] || return 0
  if same_dest "$dest" "$DEST_APP"; then
    return 0
  fi
  refresh_bundle_binaries "$dest"
}

refresh_if_other "$HOME/Applications/Prismattyc.app"
refresh_if_other "/Applications/Prismattyc.app"
refresh_if_other "$ROOT/target/Prismattyc.app"
if command -v mdfind >/dev/null 2>&1; then
  while IFS= read -r found; do
    [[ -n "$found" ]] || continue
    refresh_if_other "$found"
  done < <(mdfind "kMDItemCFBundleIdentifier == '$BUNDLE_ID'" 2>/dev/null || true)
fi

echo "Launch from Finder, Spotlight, or the Dock. cargo install prismattyc-host"
echo "is a separate binary (~/.cargo/bin) and is not what Prismattyc.app runs."
if pgrep -x prismattyc-host >/dev/null 2>&1; then
  echo "warning: the host is still running. Quit the app (Cmd+Q), then reopen," >&2
  echo "         or the Dock keeps the old image." >&2
fi
