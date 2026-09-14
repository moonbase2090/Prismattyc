#!/usr/bin/env bash
# Offline ElevenLabs walkthrough clips (PT-197).
# Reads captions from crates/prismattyc-mux/walkthrough/levels.toml.
# Writes crates/prismattyc-host/assets/walkthrough/<id>.ogg and manifest.json.
#
# Usage:
#   ELEVENLABS_API_KEY=... ./scripts/walkthrough-voice.sh [--voice NAME] [--force] [--dry-run]
#
# Default voice is Russ. The voice id is resolved at run time. Never hard-code it.
# Fails the whole run on any clip error. Does not leave a partial set in dest.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CATALOG="$ROOT/crates/prismattyc-mux/walkthrough/levels.toml"
DEST="$ROOT/crates/prismattyc-host/assets/walkthrough"
MANIFEST="$DEST/manifest.json"
MODEL="eleven_multilingual_v2"
OUTPUT_FORMAT="mp3_22050_32"
MAX_BYTES=$((100 * 1024))
VOICE_FLAG=""
FORCE=0
DRY_RUN=0

usage() {
  echo "usage: $0 [--voice NAME] [--force] [--dry-run]" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --voice)
      [ $# -ge 2 ] || usage
      VOICE_FLAG="$2"
      shift 2
      ;;
    --force)
      FORCE=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      ;;
    *)
      usage
      ;;
  esac
done

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "walkthrough-voice: missing $1" >&2
    exit 1
  }
}

# Apple ships Python 3.9. Prefer an interpreter with TOML support; do not
# install packages into the system interpreter.
PYTHON_BIN=""
for candidate in python3 python3.14 python3.13 python3.12 python3.11 /opt/homebrew/bin/python3 /usr/local/bin/python3; do
  if "$candidate" -c 'try:
 import tomllib
except ImportError:
 import tomli' >/dev/null 2>&1; then
    PYTHON_BIN="$candidate"
    break
  fi
done
if [ -z "$PYTHON_BIN" ]; then
  echo "walkthrough-voice: install Python 3.11 or newer, or tomli for python3" >&2
  exit 1
fi
need jq

config_toml() {
  if [ -n "${PRISMATTYC_CONFIG:-}" ]; then
    printf '%s\n' "$PRISMATTYC_CONFIG"
  elif [ -n "${XDG_CONFIG_HOME:-}" ] && [ -f "$XDG_CONFIG_HOME/prismattyc/config.toml" ]; then
    printf '%s\n' "$XDG_CONFIG_HOME/prismattyc/config.toml"
  elif [ -n "${HOME:-}" ] && [ -f "$HOME/.config/prismattyc/config.toml" ]; then
    printf '%s\n' "$HOME/.config/prismattyc/config.toml"
  fi
}

config_voice() {
  local file
  file="$(config_toml)"
  [ -n "$file" ] && [ -f "$file" ] || return 0
  grep -E '^walkthrough_voice[[:space:]]*=[[:space:]]*"' "$file" 2>/dev/null \
    | head -n 1 \
    | sed -E 's/^walkthrough_voice[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/'
}

if [ -n "$VOICE_FLAG" ]; then
  VOICE="$VOICE_FLAG"
  VOICE_SOURCE=flag
else
  CONFIG_VOICE="$(config_voice || true)"
  if [ -n "$CONFIG_VOICE" ]; then
    VOICE="$CONFIG_VOICE"
    VOICE_SOURCE=config
  else
    VOICE="Russ"
    VOICE_SOURCE=default
  fi
fi

list_clips() {
  "$PYTHON_BIN" - "$CATALOG" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
text = path.read_text()
try:
    import tomllib
except ImportError:
    import tomli as tomllib  # type: ignore

data = tomllib.loads(text)
intro = data.get("intro") or "Welcome to the Prismattyc walkthrough."
print(f"intro\t{intro}")
for level in data.get("level", []):
    for step in level.get("step", []):
        step_id = step.get("audio") or step.get("id")
        caption = step.get("caption") or ""
        if step_id:
            print(f"{step_id}\t{caption}")
PY
}

caption_hash() {
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s' "$1" | sha256sum | awk '{print $1}'
  else
    printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
  fi
}

if [ "$DRY_RUN" -eq 1 ]; then
  echo "voice=$VOICE source=$VOICE_SOURCE model=$MODEL dest=$DEST"
  list_clips | while IFS=$'\t' read -r step_id caption; do
    echo "$step_id	$caption"
  done
  exit 0
fi

need curl
need ffmpeg

if [ -z "${ELEVENLABS_API_KEY:-}" ]; then
  echo "walkthrough-voice: set ELEVENLABS_API_KEY" >&2
  exit 1
fi

if [ ! -f "$CATALOG" ]; then
  echo "walkthrough-voice: catalog missing: $CATALOG" >&2
  exit 1
fi

voices_json="$(curl -sS --fail https://api.elevenlabs.io/v1/voices \
  -H "xi-api-key: ${ELEVENLABS_API_KEY}")"
voice_id="$(printf '%s' "$voices_json" | jq -r --arg name "$VOICE" '
  (.voices // [])[]
  | select((.name | ascii_downcase) == ($name | ascii_downcase))
  | .voice_id' | head -n 1)"
if [ -z "$voice_id" ] || [ "$voice_id" = "null" ]; then
  echo "walkthrough-voice: voice not found: $VOICE" >&2
  exit 1
fi

old_manifest="{}"
if [ -f "$MANIFEST" ]; then
  old_manifest="$(cat "$MANIFEST")"
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/walkthrough-voice.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
stage="$tmp/out"
mkdir -p "$stage"

fail=0
clips_json='[]'

while IFS=$'\t' read -r step_id caption; do
  [ -n "$step_id" ] || continue
  hash="$(caption_hash "$caption")"
  existing_hash="$(printf '%s' "$old_manifest" | jq -r --arg id "$step_id" '
    (.clips // [])[] | select(.step_id == $id) | .caption_hash // empty' | head -n 1)"
  existing_path="$(printf '%s' "$old_manifest" | jq -r --arg id "$step_id" '
    (.clips // [])[] | select(.step_id == $id) | .path // empty' | head -n 1)"
  if [ "$FORCE" -eq 0 ] && [ -n "$existing_hash" ] && [ "$existing_hash" = "$hash" ] \
    && [ -n "$existing_path" ] && [ -f "$DEST/$existing_path" ]; then
    cp "$DEST/$existing_path" "$stage/$existing_path"
    clips_json="$(printf '%s' "$clips_json" | jq --arg id "$step_id" --arg path "$existing_path" \
      --arg vid "$voice_id" --arg hash "$hash" \
      '. + [{step_id:$id, path:$path, voice_id:$vid, caption_hash:$hash}]')"
    echo "skip $step_id (caption unchanged)"
    continue
  fi

  mp3="$tmp/${step_id}.mp3"
  payload="$(jq -n --arg text "$caption" --arg model "$MODEL" \
    '{text:$text, model_id:$model}')"
  if ! curl -sS --fail \
    "https://api.elevenlabs.io/v1/text-to-speech/${voice_id}?output_format=${OUTPUT_FORMAT}" \
    -H "xi-api-key: ${ELEVENLABS_API_KEY}" \
    -H "Content-Type: application/json" \
    -d "$payload" \
    -o "$mp3"; then
    echo "walkthrough-voice: tts failed for $step_id" >&2
    fail=1
    break
  fi
  ogg="$stage/${step_id}.ogg"
  if ! ffmpeg -y -loglevel error -i "$mp3" -c:a libvorbis -q:a 0 "$ogg"; then
    echo "walkthrough-voice: ffmpeg failed for $step_id" >&2
    fail=1
    break
  fi
  use_path="${step_id}.ogg"
  use_file="$ogg"
  ogg_size="$(wc -c <"$ogg")"
  mp3_size="$(wc -c <"$mp3")"
  if [ "$mp3_size" -lt "$ogg_size" ]; then
    cp "$mp3" "$stage/${step_id}.mp3"
    use_path="${step_id}.mp3"
    use_file="$stage/${step_id}.mp3"
    rm -f "$ogg"
  fi
  size="$(wc -c <"$use_file")"
  if [ "$size" -gt "$MAX_BYTES" ]; then
    echo "walkthrough-voice: $step_id is ${size} bytes (max $MAX_BYTES)" >&2
    fail=1
    break
  fi
  clips_json="$(printf '%s' "$clips_json" | jq --arg id "$step_id" --arg path "$use_path" \
    --arg vid "$voice_id" --arg hash "$hash" \
    '. + [{step_id:$id, path:$path, voice_id:$vid, caption_hash:$hash}]')"
  echo "wrote $use_path ($size bytes)"
done < <(list_clips)

if [ "$fail" -ne 0 ]; then
  echo "walkthrough-voice: aborting; destination unchanged" >&2
  exit 1
fi

mkdir -p "$DEST"
# Replace clip files: copy staged set, then drop dest files not in the new set.
find "$DEST" -maxdepth 1 \( -name '*.ogg' -o -name '*.mp3' \) -delete
cp -a "$stage"/. "$DEST"/
jq -n --arg voice "$VOICE" --arg vid "$voice_id" --arg model "$MODEL" --argjson clips "$clips_json" \
  '{voice:$voice, voice_id:$vid, model:$model, clips:$clips}' >"$MANIFEST"
echo "wrote $MANIFEST"
