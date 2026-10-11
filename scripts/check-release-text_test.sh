#!/usr/bin/env bash
# The checker fails when a forbidden marker is present, and its output is only file:line.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
checker="$root/scripts/check-release-text.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

home="$work/home"
mkdir -p "$home"
export HOME="$home"
unset RELEASE_DENYLIST
unset RELEASE_DENYLIST_FILE

clean_tree() {
  local dir="$1"
  mkdir -p "$dir/docs"
  printf '## [0.0.1]\n\nA public note.\n' >"$dir/CHANGELOG.md"
  printf '# Notes\n\nNothing private here.\n' >"$dir/README.md"
  printf 'Page/Home/End stays prose.\nbackground@2x.png is an asset name.\n' >"$dir/docs/sample.md"
}

assert_clean() {
  local dir="$1"
  local out err code=0
  out="$(mktemp)"
  err="$(mktemp)"
  "$checker" --root "$dir" >"$out" 2>"$err" || code=$?
  if [[ "$code" -ne 0 || -s "$out" || -s "$err" ]]; then
    echo "expected a clean tree to pass" >&2
    cat "$out" "$err" >&2
    exit 1
  fi
  rm -f "$out" "$err"
}

assert_hit() {
  local dir="$1"
  local forbidden="$2"
  local out err code=0
  out="$(mktemp)"
  err="$(mktemp)"
  "$checker" --root "$dir" >"$out" 2>"$err" || code=$?
  if [[ "$code" -eq 0 ]]; then
    echo "expected a hit" >&2
    exit 1
  fi
  if [[ ! -s "$out" ]]; then
    echo "expected file:line output" >&2
    exit 1
  fi
  if grep -F -q -- "$forbidden" "$out" "$err"; then
    echo "report included matched text" >&2
    exit 1
  fi
  if grep -E -v -q '^[^:]+:[0-9]+$' "$out"; then
    echo "report was not file:line" >&2
    cat "$out" >&2
    exit 1
  fi
  rm -f "$out" "$err"
}

clean_tree "$work/clean"
printf 'Contact 322824348+mb2090@users.noreply.github.com for the release.\n' >>"$work/clean/README.md"
assert_clean "$work/clean"

clean_tree "$work/desktop"
printf 'Machine DESKTOP-ABC123 was used.\n' >>"$work/desktop/CHANGELOG.md"
assert_hit "$work/desktop" "DESKTOP-ABC123"

clean_tree "$work/users"
printf 'See /Users/someone/notes.\n' >>"$work/users/README.md"
assert_hit "$work/users" "/Users/someone"

clean_tree "$work/home-path"
printf 'Built under /home/runner/work.\n' >>"$work/home-path/docs/sample.md"
assert_hit "$work/home-path" "/home/runner"

clean_tree "$work/win"
printf 'C:\\Users\\someone\\AppData\n' >>"$work/win/CHANGELOG.md"
assert_hit "$work/win" 'C:\\Users\\someone'

clean_tree "$work/mail"
printf 'Write someone@example.com for help.\n' >>"$work/mail/README.md"
assert_hit "$work/mail" "someone@example.com"

clean_tree "$work/token"
printf 'token ghp_aaaaaaaaaaaaaaaaaaaa leaked\n' >>"$work/token/CHANGELOG.md"
assert_hit "$work/token" "ghp_aaaaaaaaaaaaaaaaaaaa"

clean_tree "$work/ip"
printf 'peer 192.168.1.20\n' >>"$work/ip/docs/sample.md"
assert_hit "$work/ip" "192.168.1.20"

clean_tree "$work/public-ip"
printf 'resolver 8.8.8.8\n' >>"$work/public-ip/docs/sample.md"
assert_clean "$work/public-ip"

clean_tree "$work/deny"
printf 'marker ZZZTOKEN in the notes\n' >>"$work/deny/CHANGELOG.md"
export RELEASE_DENYLIST="ZZZTOKEN"
assert_hit "$work/deny" "ZZZTOKEN"
unset RELEASE_DENYLIST

clean_tree "$work/missing-deny"
assert_clean "$work/missing-deny"

clean_tree "$work/bad-deny"
export RELEASE_DENYLIST="("
out="$(mktemp)"
err="$(mktemp)"
code=0
"$checker" --root "$work/bad-deny" >"$out" 2>"$err" || code=$?
if [[ "$code" -eq 0 ]]; then
  echo "invalid denylist pattern must fail" >&2
  exit 1
fi
if grep -F -q -- "(" "$err"; then
  echo "invalid pattern was echoed" >&2
  exit 1
fi
if ! grep -F -q "invalid pattern" "$err"; then
  echo "missing invalid-pattern notice" >&2
  exit 1
fi
unset RELEASE_DENYLIST
rm -f "$out" "$err"

artifact="$work/artifact.bin"
printf 'prefix\0DESKTOP-ZZZZ9999\0suffix\n' >"$artifact"
clean_tree "$work/bin"
out="$(mktemp)"
code=0
"$checker" --root "$work/bin" --artifact "$artifact" >"$out" 2>"$work/bin.err" || code=$?
if [[ "$code" -eq 0 ]]; then
  echo "artifact marker must fail" >&2
  exit 1
fi
if grep -F -q "DESKTOP-ZZZZ9999" "$out" "$work/bin.err"; then
  echo "artifact report included matched text" >&2
  exit 1
fi
if ! grep -q "^${artifact}:[0-9][0-9]*$" "$out"; then
  echo "artifact report was not file:line" >&2
  cat "$out" >&2
  exit 1
fi

assert_artifact_hit() {
  local label="$1"
  local path="$2"
  local forbidden="$3"
  clean_tree "$work/$label"
  local out err code=0
  out="$(mktemp)"
  err="$(mktemp)"
  "$checker" --root "$work/$label" --artifact "$path" >"$out" 2>"$err" || code=$?
  if [[ "$code" -eq 0 ]]; then
    echo "$label: planted marker must fail" >&2
    exit 1
  fi
  if grep -F -q -- "$forbidden" "$out" "$err"; then
    echo "$label: report included matched text" >&2
    exit 1
  fi
  if ! grep -q "^${path}:[0-9][0-9]*$" "$out"; then
    echo "$label: report was not file:line" >&2
    cat "$out" "$err" >&2
    exit 1
  fi
  rm -f "$out" "$err"
}

marker="/Users/privateuser/secret"
python3 - "$work" <<'PY'
import io
import pathlib
import tarfile
import zipfile

work = pathlib.Path(__import__("sys").argv[1])
marker = "/Users/privateuser/secret"
desktop = "DESKTOP-SECRET123"

# Plain ZIP member
zip_path = work / "planted.zip"
with zipfile.ZipFile(zip_path, "w") as archive:
    archive.writestr("notes.txt", f"path {marker}\n")

# tar.gz member
tar_path = work / "planted.tar.gz"
with tarfile.open(tar_path, "w:gz") as archive:
    data = f"path {marker}\n".encode()
    info = tarfile.TarInfo(name="notes.txt")
    info.size = len(data)
    archive.addfile(info, io.BytesIO(data))

# Nested ZIP inside tar.gz
nested_path = work / "nested.tar.gz"
inner = io.BytesIO()
with zipfile.ZipFile(inner, "w") as archive:
    archive.writestr("inner.txt", f"path {marker}\n")
inner_bytes = inner.getvalue()
with tarfile.open(nested_path, "w:gz") as archive:
    info = tarfile.TarInfo(name="inner.zip")
    info.size = len(inner_bytes)
    archive.addfile(info, io.BytesIO(inner_bytes))

# ZIP comment
comment_path = work / "comment.zip"
with zipfile.ZipFile(comment_path, "w") as archive:
    archive.writestr("ok.txt", "clean\n")
    archive.comment = marker.encode()

# ZIP symlink target
symlink_zip = work / "symlink.zip"
with zipfile.ZipFile(symlink_zip, "w") as archive:
    info = zipfile.ZipInfo("link")
    info.create_system = 3
    info.external_attr = (0o120777 << 16)
    archive.writestr(info, marker)

# tar.gz symlink target
symlink_tar = work / "symlink.tar.gz"
with tarfile.open(symlink_tar, "w:gz") as archive:
    info = tarfile.TarInfo(name="link")
    info.type = tarfile.SYMTYPE
    info.linkname = marker
    archive.addfile(info)

# UTF-16LE wide string in a binary blob
utf16_path = work / "utf16le.bin"
payload = b"pad\0" + desktop.encode("utf-16le") + b"\0\0tail"
utf16_path.write_bytes(payload)
PY

assert_artifact_hit "zip" "$work/planted.zip" "$marker"
assert_artifact_hit "targz" "$work/planted.tar.gz" "$marker"
assert_artifact_hit "nested" "$work/nested.tar.gz" "$marker"
assert_artifact_hit "zip-comment" "$work/comment.zip" "$marker"
assert_artifact_hit "zip-symlink" "$work/symlink.zip" "$marker"
assert_artifact_hit "tar-symlink" "$work/symlink.tar.gz" "$marker"
assert_artifact_hit "utf16le" "$work/utf16le.bin" "DESKTOP-SECRET123"

# Encrypted ZIP members are uninspectable; fail closed rather than exit 0.
printf 'path %s\n' "$marker" >"$work/secret-notes.txt"
if ! zip -q -P 'test-pass' -j "$work/encrypted.zip" "$work/secret-notes.txt"; then
  echo "zip encryption helper unavailable" >&2
  exit 1
fi
clean_tree "$work/encrypted"
out="$(mktemp)"
err="$(mktemp)"
code=0
"$checker" --root "$work/encrypted" --artifact "$work/encrypted.zip" >"$out" 2>"$err" || code=$?
if [[ "$code" -eq 0 ]]; then
  echo "encrypted zip must fail closed" >&2
  cat "$out" "$err" >&2
  exit 1
fi
if ! grep -F -q "artifact could not be read" "$err"; then
  echo "encrypted zip missing fail-closed notice" >&2
  cat "$err" >&2
  exit 1
fi
rm -f "$out" "$err"

# A ZIP member larger than the per-member budget must fail closed (no silent truncate).
python3 - "$work/oversize.zip" <<'PY'
import pathlib
import sys
import zipfile

path = pathlib.Path(sys.argv[1])
payload = (b"A" * (8 * 1024 * 1024 + 64)) + b"DESKTOP-OVERSIZE1"
with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as archive:
    archive.writestr("big.bin", payload)
PY
clean_tree "$work/oversize"
out="$(mktemp)"
err="$(mktemp)"
code=0
"$checker" --root "$work/oversize" --artifact "$work/oversize.zip" >"$out" 2>"$err" || code=$?
if [[ "$code" -eq 0 ]]; then
  echo "oversize zip member must fail closed" >&2
  cat "$out" "$err" >&2
  exit 1
fi
if ! grep -F -q "artifact could not be read" "$err"; then
  echo "oversize zip missing fail-closed notice" >&2
  cat "$err" >&2
  exit 1
fi
rm -f "$out" "$err"

# Nesting beyond the depth limit must fail closed (no silent skip).
python3 - "$work/deep.zip" <<'PY'
import io
import pathlib
import sys
import zipfile

marker = b"path /Users/privateuser/secret\n"
buf = io.BytesIO()
with zipfile.ZipFile(buf, "w") as archive:
    archive.writestr("notes.txt", marker)
payload = buf.getvalue()
# Ten zip layers so an archive remains when depth reaches the cap.
for _ in range(9):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as archive:
        archive.writestr("inner.zip", payload)
    payload = buf.getvalue()
pathlib.Path(sys.argv[1]).write_bytes(payload)
PY
clean_tree "$work/deep"
out="$(mktemp)"
err="$(mktemp)"
code=0
"$checker" --root "$work/deep" --artifact "$work/deep.zip" >"$out" 2>"$err" || code=$?
if [[ "$code" -eq 0 ]]; then
  echo "deeply nested zip must fail closed" >&2
  cat "$out" "$err" >&2
  exit 1
fi
if ! grep -F -q "artifact could not be read" "$err"; then
  echo "deep zip missing fail-closed notice" >&2
  cat "$out" "$err" >&2
  exit 1
fi
rm -f "$out" "$err"

# Compressed archives must not be accepted when the marker is only in members.
# (The planted ZIP above fails; a clean ZIP still passes.)
python3 - "$work/clean-artifact.zip" <<'PY'
import pathlib, zipfile, sys
path = pathlib.Path(sys.argv[1])
with zipfile.ZipFile(path, "w") as archive:
    archive.writestr("ok.txt", "public release notes\n")
PY
clean_tree "$work/clean-zip"
out="$(mktemp)"
err="$(mktemp)"
code=0
"$checker" --root "$work/clean-zip" --artifact "$work/clean-artifact.zip" >"$out" 2>"$err" || code=$?
if [[ "$code" -ne 0 || -s "$out" || -s "$err" ]]; then
  echo "clean zip artifact must pass" >&2
  cat "$out" "$err" >&2
  exit 1
fi
rm -f "$out" "$err"

repo_out="$(mktemp)"
repo_err="$(mktemp)"
repo_code=0
"$checker" --root "$root" >"$repo_out" 2>"$repo_err" || repo_code=$?
if [[ "$repo_code" -ne 0 || -s "$repo_out" || -s "$repo_err" ]]; then
  echo "current tree matched a generic pattern" >&2
  cat "$repo_out" "$repo_err" >&2
  exit 1
fi
rm -f "$repo_out" "$repo_err"

echo "check-release-text tests passed"
