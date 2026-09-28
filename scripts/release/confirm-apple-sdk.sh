#!/usr/bin/env bash
# Confirm the active macOS SDK is new enough for current AppKit window chrome.
# Used by the Apple release job and by the PR smoke job that exercises it.
set -euo pipefail
if xcodebuild -version >/dev/null 2>&1; then
  echo "Xcode: $(xcodebuild -version | tr '\n' ' ')"
else
  echo "Xcode: (command line tools; xcodebuild not selected)"
fi
echo "SDK: $(xcrun --sdk macosx --show-sdk-version) at $(xcrun --sdk macosx --show-sdk-path)"
# Current window chrome (including glass on recent macOS) only appears when the
# binary is linked against a current SDK. Fail early if this runner is still on
# an old one. macos-26 ships Xcode 26 / SDK 26+.
sdk="$(xcrun --sdk macosx --show-sdk-version)"
major="${sdk%%.*}"
if [ "$major" -lt 26 ]; then
  echo "::error::expected macOS SDK 26+, got $sdk"
  exit 1
fi
echo "sdk_major=$major"
