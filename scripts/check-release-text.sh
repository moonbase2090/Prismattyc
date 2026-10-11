#!/usr/bin/env bash
# Fail when public release text contains private infrastructure markers.
# Output is file:line only. Matched text and denylist terms are not printed.
set -euo pipefail

usage() {
  echo "usage: check-release-text.sh [--root DIR] [--extra FILE] [--artifact PATH]" >&2
}

root=""
extras=()
artifacts=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      root="${2:-}"
      shift 2
      ;;
    --extra)
      extras+=("${2:-}")
      shift 2
      ;;
    --artifact)
      artifacts+=("${2:-}")
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$root" ]]; then
  root="$(cd "$(dirname "$0")/.." && pwd)"
fi

allow_email="322824348+mb2090@users.noreply.github.com"
# Generic patterns only. Case-insensitive except where the expression already is.
# A home-directory marker must not treat the word Home inside prose as a path.
generic_patterns=(
  'DESKTOP-[A-Z0-9]+'
  '[A-Za-z0-9-]+\.local\b'
  '/Users/'
  '(^|[^A-Za-z0-9])/home/[a-z]'
  'C:\\Users'
  '-----BEGIN [A-Z ]*PRIVATE KEY-----'
  '(ghp|gho|ghs|ghu|github_pat)_[A-Za-z0-9_]{20,}'
  '\b(10\.[0-9]+\.[0-9]+\.[0-9]+|192\.168\.[0-9]+\.[0-9]+|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]+\.[0-9]+)\b'
)
email_pattern='[A-Za-z0-9._%+-]+@[A-Za-z][A-Za-z0-9.-]*\.[A-Za-z]{2,}'

hits="$(mktemp)"
tmp="$(mktemp -d)"
trap 'rm -f "$hits"; rm -rf "$tmp"' EXIT
fail=0

report_line() {
  printf '%s:%s\n' "$1" "$2" >>"$hits"
  fail=1
}

scan_pattern() {
  local file="$1"
  local pattern="$2"
  local out code=0
  out="$(grep -n -E -I -i -- "$pattern" "$file" 2>/dev/null)" || code=$?
  if [[ "$code" -eq 1 ]]; then
    return 0
  fi
  if [[ "$code" -ne 0 ]]; then
    echo "check-release-text: invalid pattern" >&2
    fail=1
    return 0
  fi
  local hit lineno
  while IFS= read -r hit; do
    [[ -n "$hit" ]] || continue
    lineno="${hit%%:*}"
    if [[ "$lineno" =~ ^[0-9]+$ ]]; then
      report_line "$file" "$lineno"
    fi
  done <<<"$out"
}

scan_emails() {
  local file="$1"
  local out code=0
  out="$(grep -n -E -o -I -- "$email_pattern" "$file" 2>/dev/null)" || code=$?
  if [[ "$code" -eq 1 ]]; then
    return 0
  fi
  if [[ "$code" -ne 0 ]]; then
    echo "check-release-text: invalid pattern" >&2
    fail=1
    return 0
  fi
  local hit lineno match
  while IFS= read -r hit; do
    [[ -n "$hit" ]] || continue
    lineno="${hit%%:*}"
    match="${hit#*:}"
    if [[ "$lineno" =~ ^[0-9]+$ && "$match" != "$allow_email" ]]; then
      report_line "$file" "$lineno"
    fi
  done <<<"$out"
}

scan_text_file() {
  local file="$1"
  local pattern
  [[ -f "$file" ]] || return 0
  for pattern in "${generic_patterns[@]}"; do
    scan_pattern "$file" "$pattern"
  done
  scan_emails "$file"
  if [[ "${#denylist[@]}" -gt 0 ]]; then
    for pattern in "${denylist[@]}"; do
      scan_pattern "$file" "$pattern"
    done
  fi
}

load_denylist() {
  denylist=()
  local source line
  local file="${RELEASE_DENYLIST_FILE:-${HOME:-}/.config/moonbase/release-denylist.txt}"
  if [[ -n "$file" && -f "$file" ]]; then
    while IFS= read -r line || [[ -n "$line" ]]; do
      line="${line%$'\r'}"
      [[ -n "$line" ]] || continue
      denylist+=("$line")
    done <"$file"
  fi
  if [[ -n "${RELEASE_DENYLIST:-}" ]]; then
    while IFS= read -r line || [[ -n "$line" ]]; do
      line="${line%$'\r'}"
      [[ -n "$line" ]] || continue
      denylist+=("$line")
    done <<<"${RELEASE_DENYLIST}"
  fi
}

collect_text_files() {
  text_files=()
  local path
  shopt -s nullglob
  for path in "$root"/CHANGELOG.md "$root"/CHANGELOG*.md "$root"/README* "$root"/release-notes*; do
    [[ -f "$path" ]] || continue
    text_files+=("$path")
  done
  shopt -u nullglob
  if [[ -d "$root/docs" ]]; then
    while IFS= read -r path; do
      case "$path" in
        *.md | *.markdown | *.txt | *.rst | *.html)
          text_files+=("$path")
          ;;
      esac
    done < <(find "$root/docs" -type f)
  fi
  for path in "${extras[@]}"; do
    [[ -n "$path" && -f "$path" ]] || continue
    text_files+=("$path")
  done
}

# Expand an artifact into printable text for scanning: ASCII strings, UTF-16LE
# strings, ZIP/tar.gz member payloads (including nested archives), symlink
# targets, and archive / member comments. Writes UTF-8 text to $2.
expand_artifact_text() {
  local file="$1"
  local out="$2"
  python3 - "$file" "$out" <<'PY'
import io
import pathlib
import sys
import tarfile
import zipfile

src = pathlib.Path(sys.argv[1])
out = pathlib.Path(sys.argv[2])
MAX_DEPTH = 8
MAX_BYTES = 32 * 1024 * 1024
MAX_MEMBER = 8 * 1024 * 1024
PRINTABLE = set(range(0x20, 0x7F)) | {0x09, 0x0A, 0x0D}
chunks: list[str] = []
total = 0


class ArtifactError(Exception):
    """An archive member could not be inspected."""


def add_text(text: str) -> None:
    global total
    if not text or total >= MAX_BYTES:
        return
    encoded = text.encode("utf-8", "replace")
    remain = MAX_BYTES - total
    if len(encoded) > remain:
        encoded = encoded[:remain]
    total += len(encoded)
    if encoded:
        chunks.append(encoded.decode("utf-8", "replace"))


def read_limited(stream, limit: int) -> bytes:
    """Read at most ``limit`` bytes so compressed bombs cannot exhaust memory."""
    if limit <= 0:
        return b""
    parts: list[bytes] = []
    remaining = limit
    while remaining > 0:
        block = stream.read(min(65536, remaining))
        if not block:
            break
        parts.append(block)
        remaining -= len(block)
    return b"".join(parts)


def ascii_strings(data: bytes, minimum: int = 6) -> None:
    start = None
    for index, byte in enumerate(data):
        if byte in PRINTABLE:
            if start is None:
                start = index
            continue
        if start is not None and index - start >= minimum:
            add_text(data[start:index].decode("ascii", "replace"))
        start = None
    if start is not None and len(data) - start >= minimum:
        add_text(data[start:].decode("ascii", "replace"))


def utf16le_strings(data: bytes, minimum: int = 6) -> None:
    # Walk even offsets; also odd offsets so an unaligned payload is still found.
    for offset in (0, 1):
        if offset >= len(data):
            continue
        chars: list[str] = []
        i = offset
        while i + 1 < len(data):
            code = data[i] | (data[i + 1] << 8)
            i += 2
            if 0x20 <= code < 0x7F or code in (0x09, 0x0A, 0x0D):
                chars.append(chr(code))
                continue
            if len(chars) >= minimum:
                add_text("".join(chars))
            chars = []
        if len(chars) >= minimum:
            add_text("".join(chars))


def safe_name(name: str) -> bool:
    if name.startswith("/") or name.startswith("\\"):
        return False
    return ".." not in pathlib.PurePosixPath(name).parts


def zip_required(name: str) -> bool:
    lowered = name.lower()
    return lowered.endswith((".zip", ".whl"))


def tar_required(name: str) -> bool:
    lowered = name.lower()
    return lowered.endswith((".tar.gz", ".tgz", ".tar"))


def looks_like_zip(name: str, data: bytes) -> bool:
    if zip_required(name):
        return True
    return len(data) >= 4 and data[:2] == b"PK"


def looks_like_tar_gz(name: str, data: bytes) -> bool:
    if tar_required(name):
        return True
    return len(data) >= 2 and data[:2] == b"\x1f\x8b"


def scan_bytes(name: str, data: bytes, depth: int) -> None:
    if total >= MAX_BYTES:
        return
    add_text(name)
    ascii_strings(data)
    utf16le_strings(data)
    if depth >= MAX_DEPTH:
        return
    if looks_like_zip(name, data):
        scan_zip_bytes(data, depth + 1, required=zip_required(name))
    elif looks_like_tar_gz(name, data):
        scan_tar_bytes(data, name, depth + 1, required=tar_required(name))


def member_budget() -> int:
    remain = MAX_BYTES - total
    if remain <= 0:
        return 0
    return min(MAX_MEMBER, remain)


def scan_zip_bytes(data: bytes, depth: int, required: bool) -> None:
    try:
        archive = zipfile.ZipFile(io.BytesIO(data))
    except (OSError, zipfile.BadZipFile):
        if required:
            raise ArtifactError("zip could not be opened")
        return
    with archive:
        if archive.comment:
            add_text(archive.comment.decode("utf-8", "replace"))
        for info in archive.infolist():
            if info.comment:
                add_text(info.comment.decode("utf-8", "replace"))
            add_text(info.filename)
            # Symlink target is stored as the member payload for zip symlinks.
            is_symlink = (info.external_attr >> 16) & 0o170000 == 0o120000
            if info.is_dir() and not is_symlink:
                continue
            if not safe_name(info.filename):
                raise ArtifactError("zip member path rejected")
            try:
                with archive.open(info) as handle:
                    payload = read_limited(handle, member_budget())
            except Exception as exc:
                raise ArtifactError("zip member could not be read") from exc
            if is_symlink:
                add_text(payload.decode("utf-8", "replace"))
                continue
            scan_bytes(info.filename, payload, depth)


def scan_tar_bytes(data: bytes, name: str, depth: int, required: bool) -> None:
    lowered = name.lower()
    if lowered.endswith((".tar.gz", ".tgz")) or data[:2] == b"\x1f\x8b":
        mode = "r:gz"
    else:
        mode = "r:"
    try:
        archive = tarfile.open(fileobj=io.BytesIO(data), mode=mode)
    except (OSError, tarfile.TarError):
        if required:
            raise ArtifactError("tar could not be opened")
        return
    with archive:
        for member in archive.getmembers():
            add_text(member.name)
            if member.issym() or member.islnk():
                add_text(member.linkname or "")
                continue
            if not member.isfile():
                continue
            if not safe_name(member.name):
                raise ArtifactError("tar member path rejected")
            extracted = archive.extractfile(member)
            if extracted is None:
                raise ArtifactError("tar member could not be read")
            try:
                payload = read_limited(extracted, member_budget())
            except Exception as exc:
                raise ArtifactError("tar member could not be read") from exc
            scan_bytes(member.name, payload, depth)


try:
    raw = src.read_bytes()
    scan_bytes(src.name, raw, 0)
except ArtifactError as exc:
    sys.stderr.write("check-release-text: artifact could not be read\n")
    sys.exit(2)
except OSError:
    sys.stderr.write("check-release-text: artifact could not be read\n")
    sys.exit(2)
out.write_text("\n".join(chunks) + ("\n" if chunks else ""), encoding="utf-8")
PY
}

scan_artifact() {
  local file="$1"
  local strings_file="$tmp/strings.$$.$RANDOM"
  [[ -f "$file" ]] || return 0
  if ! command -v python3 >/dev/null 2>&1; then
    echo "check-release-text: python3 is required to scan artifacts" >&2
    fail=1
    return 0
  fi
  if ! expand_artifact_text "$file" "$strings_file"; then
    echo "check-release-text: artifact could not be read" >&2
    fail=1
    rm -f "$strings_file"
    return 0
  fi
  local isolated old_hits
  isolated="$(mktemp)"
  old_hits="$hits"
  hits="$isolated"
  scan_text_file "$strings_file"
  hits="$old_hits"
  if [[ -s "$isolated" ]]; then
    local hit
    while IFS= read -r hit; do
      printf '%s:%s\n' "$file" "${hit#"$strings_file:"}" >>"$hits"
    done <"$isolated"
  fi
  rm -f "$isolated" "$strings_file"
}

scan_artifact_path() {
  local path="$1"
  if [[ -d "$path" ]]; then
    local file
    while IFS= read -r file; do
      scan_artifact "$file"
    done < <(find "$path" -type f)
  else
    scan_artifact "$path"
  fi
}

load_denylist
collect_text_files
for path in "${text_files[@]}"; do
  scan_text_file "$path"
done
for path in "${artifacts[@]}"; do
  [[ -n "$path" ]] || continue
  scan_artifact_path "$path"
done

if [[ -s "$hits" ]]; then
  sort -u "$hits"
fi
if [[ "$fail" -ne 0 ]]; then
  exit 1
fi
exit 0
