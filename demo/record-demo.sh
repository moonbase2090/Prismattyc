#!/bin/bash
# Prismattyc demo reel (Linux / Docker box): drives prismattyc-host through a
# feature tour with xdotool, records the display with ffmpeg x11grab, and lays
# one ElevenLabs narration clip per beat at the moment that beat actually
# started. Beats with agents in them take as long as the agents take; the
# narration follows the recorded timestamps instead of the other way round.
#
# Output: ~/Desktop/demo-reel.mp4 (1920x1080, H.264 + AAC)
#
# Modes:
#   --check   tool + window check (no key needed)
#   --launch  start the host on X11 and leave it up
#   --dry     silent take: no ElevenLabs, fixed per-beat pauses
#   --clips   generate (or reuse) the narration clips only; no recording
#   (none)    full take with narration
set -euo pipefail
export PATH="/usr/local/bin:$HOME/.local/bin:$HOME/.cargo/bin:/usr/bin:$PATH"
export DISPLAY="${DISPLAY:-:99}"

DIR="${PRISMATTYC_DEMO_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
PARTS="$DIR/parts"
NARRDIR="${NARRDIR:-$DIR/narr}"
FULL="$HOME/Desktop/demo-reel_full.mp4"
OUT="$HOME/Desktop/demo-reel.mp4"
VOICE_NAME="${VOICE_NAME:-Daniel}"
ELEVEN_VOICE_ID="${ELEVEN_VOICE_ID:-pH8TIDBxKcsLhKFzhwgP}"
ELEVEN_MODEL="eleven_multilingual_v2"
ELEVEN_SPEED="${ELEVEN_SPEED:-1.03}"
ENV_FILE="$DIR/.eleven.env"
HOST_LOG=/tmp/prismattyc-demo-host.log
LAUNCHED_PID=""
DRY=0
CLIPS_ONLY=0

# Narration, one line per beat, in on-screen order. Spoken only; never typed.
# Pronouncer notes: "pmux" is one word, spoken "pee-mux" — write it that way in
# every line. "a11y" is spoken "accessibility". Keep product names as words,
# never spelled out.
LINES=(
"This is Prismattyc, a terminal and multiplexer built in Rust. You can organize your work into spaces, keep sessions running when you close a window, and let coding agents talk to each other. Let's take a look."
"Hold Control and Shift, and the shortcuts appear along the bottom. It's a handy reminder when you're getting started, or when you've forgotten a key."
"You can also open the command palette and type what you're looking for. Let's find the theme picker."
"As you move through the themes, the window updates right away. Pick one you like, press Enter, and carry on."
"Color and text are where a terminal earns its keep. Here's true color, followed by wide characters, emoji, and combining marks."
"And here's box drawing, blocks, braille, and powerline symbols. They line up with the cell grid, so borders and shapes fit together cleanly."
"You can put images in the terminal too. This one uses the Kitty graphics protocol, right alongside the text."
"When output scrolls past, you can go back through the history. Control Shift F opens search. Type a few characters, and jump to a match."
"Let's make some room. Split the window, move between panes with Alt and the arrows, or switch to a grid. Need a closer look? Zoom a pane, then bring the others back."
"Tabs help separate the work. I'll name these build and logs. In a tab with several panes, the small handles let you see which pane is which."
"You can name a pane as well. I'll call this one editor. That name stays with its handle, so it's easier to find later."
"The handles also show activity. This pane is printing output, and its indicator stays busy while I work somewhere else."
"Behind the window, pee-mux keeps the sessions running. Closing a viewer doesn't stop them. You can come back later and pick up where you left off."
"Here's that in a plain terminal. I'll attach to a session, leave a message, detach, and attach again. The message is still there, and the same shell is still running."
"Save an arrangement as a space, and it gets a place on the rail. You can keep separate layouts for different jobs. The settings let you move the rail to any edge, turn on autosave, and choose how new terminals open."
"Now let's bring in two coding agents: Claude Code and Keero. Each has its own session and mailbox. Both can use the pee-mux tools through MCP."
"I'll ask Claude to send Keero a short question. Claude calls the mail tool, and pee-mux delivers the letter."
"Over in Keero's pane, the mail indicator appears and the doorbell brings the message to its attention. Keero can read it without me copying anything across."
"Keero reads the question and sends a reply. It then marks the original letter as handled, so it won't be picked up again."
"And here's the answer back in Claude's pane. That's the round trip: send, read, reply, and finish. Each agent stays in its own session."
"There are screen reader features too. AccessKit exposes the tabs, pane names, and controls, along with the focused terminal, to supported screen readers."
"Announcements can report incoming mail and requests for your attention. You can keep those on, or turn them off while leaving the accessibility tree available."
"For a quick, intentional interaction, you can write directly to a pane. Here I'm sending a shell command, checking the output, then removing and killing the test session. The receipt tells us the input was queued; the output shows what actually happened."
"That's Prismattyc. One place for your terminals, your spaces, and the agents working alongside you. Thanks for taking a look."
)
# Trailing pause after each clip. Long pauses cover long on-screen actions.
PAUSE=(1.6 1.1 1.3 1.8 1.8 2.3 2.8 2.8 3.3 3.5 3.0 4.5 2.5 4.3 5.3 2.5 3.3 3.3 4.0 6.0 3.0 3.0 3.0 4.3)
# Dry-run stand-in for each clip's spoken length (seconds).
DRYDUR=(11 11 8 10 11 14 6 8 12 9 8 8 11 10 11 13 6 12 7 11 32 24 19 10)

have() { command -v "$1" >/dev/null 2>&1; }
log() { printf '[%6.1f] %s\n' "$(elapsed)" "$*"; }

find_host_window() {
  local ids pid
  ids="$(xdotool search --onlyvisible --class prismattyc-host 2>/dev/null || true)"
  if [[ -n "${LAUNCHED_PID:-}" ]]; then
    for id in $ids; do
      pid="$(xdotool getwindowpid "$id" 2>/dev/null || true)"
      [[ "$pid" == "$LAUNCHED_PID" ]] && { echo "$id"; return 0; }
    done
  fi
  echo "$ids" | tail -1 || true
}

check_setup() {
  local missing=0
  echo "Prismattyc demo recorder check"
  echo "  DISPLAY=$DISPLAY  grab=${PRISMATTYC_DEMO_GRAB:-window}"
  for cmd in ffmpeg ffprobe fc-match xdotool python3 curl file prismattyc-host prismattyc pmux pmuxd pmux-attach pmux-mcp claude kiro-cli; do
    if have "$cmd"; then printf '  OK   %s\n' "$cmd"; else printf '  MISS %s\n' "$cmd"; missing=1; fi
  done
  xdotool getdisplaygeometry >/dev/null 2>&1 && echo "  OK   X display $(xdotool getdisplaygeometry)" || { echo "  MISS X display"; missing=1; }
  pmux ls >/dev/null 2>&1 && echo "  OK   pmux daemon: $(pmux ls 2>/dev/null | wc -l) session lines" || { echo "  MISS pmux daemon"; missing=1; }
  for f in ansi.sh colors.sh unicode.sh box.sh image.sh; do
    [[ -x "$PARTS/$f" ]] || { echo "  MISS $PARTS/$f"; missing=1; }
  done
  echo "  OK   test cards"
  if [[ -f "$ENV_FILE" ]] && grep -q '^export ELEVENLABS_API_KEY=.\+' "$ENV_FILE"; then
    echo "  OK   $ENV_FILE has an API key"
  else
    echo "  WAIT $ENV_FILE missing (--dry still works)"
  fi
  timeout 30 kiro-cli whoami >/dev/null 2>&1 && echo "  OK   kiro-cli logged in" || echo "  WARN kiro-cli not logged in"
  [[ -f "$HOME/.claude/.credentials.json" ]] && echo "  OK   claude credentials present" || echo "  WARN no claude credentials"
  [[ "$missing" -eq 0 ]] && { echo "READY"; return 0; }
  echo "FAIL"; return 1
}

release_mods() { xdotool keyup shift ctrl alt super 2>/dev/null || true; }
cleanup() {
  local status=$?
  if [[ "$CLIPS_ONLY" -eq 0 ]]; then
    cp -f "$HOST_LOG" "$HOME/Desktop/demo-host.log" 2>/dev/null || true
    local mux_log
    mux_log="$(pmux status 2>/dev/null | awk '/^log:/{print $2}')"
    [[ -z "$mux_log" ]] || cp -f "$mux_log" "$HOME/Desktop/demo-pmux.log" 2>/dev/null || true
    if [[ "$status" -ne 0 ]]; then
      ffmpeg -v error -f x11grab -video_size 1920x1080 -i "$DISPLAY" -frames:v 1 -threads 1 -update 1 "$HOME/Desktop/demo-failure.png" 2>/dev/null || true
    fi
  fi
  [[ "$CLIPS_ONLY" -eq 1 ]] || release_mods
  [[ -n "${WATCH_PID:-}" ]] && kill "$WATCH_PID" 2>/dev/null || true
  if [[ -n "${REC_PID:-}" ]]; then kill -INT "$REC_PID" 2>/dev/null || true; wait "$REC_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

launch_host() {
  log "launch prismattyc-host on X11"
  mkdir -p "$HOME/Desktop"
  ( cd "$HOME/work" 2>/dev/null || cd "$HOME"; env -u WAYLAND_DISPLAY -u TERM COLORTERM=truecolor WINIT_UNIX_BACKEND=x11 DISPLAY="$DISPLAY" \
    prismattyc-host >"$HOST_LOG" 2>&1 & echo $! >/tmp/demo-host.pid )
  LAUNCHED_PID="$(cat /tmp/demo-host.pid)"
}

wait_for_window() {
  local i wid=""
  for i in $(seq 1 60); do
    wid="$(find_host_window)"
    [[ -n "$wid" ]] && { echo "$wid"; return 0; }
    sleep 0.2
  done
  echo "ERROR: no prismattyc-host window on $DISPLAY" >&2; tail -20 "$HOST_LOG" >&2 || true
  return 1
}

activate() {
  local wid; wid="$(find_host_window)"
  [[ -z "$wid" ]] && { echo "ERROR: host window gone" >&2; return 1; }
  xdotool windowactivate --sync "$wid"; xdotool windowfocus --sync "$wid"; sleep 0.08
}

size_host() {
  local wid; wid="$(find_host_window)"
  xdotool windowactivate --sync "$wid"
  xdotool windowstate --add MAXIMIZED_VERT "$wid" 2>/dev/null || true
  xdotool windowstate --add MAXIMIZED_HORZ "$wid" 2>/dev/null || true
  sleep 0.3
}

window_geom() { xdotool getwindowgeometry --shell "$(find_host_window)"; }

type_str() { release_mods; sleep 0.15; xdotool type --delay 12 -- "$1"; }
enter() { xdotool key --clearmodifiers Return; }
type_cmd() { type_str "$1"; sleep 0.25; enter; }
key() { xdotool key --clearmodifiers "$@"; }
hold_ctrl_shift() { xdotool keydown ctrl; xdotool keydown shift; }
release_ctrl_shift() { xdotool keyup shift; xdotool keyup ctrl; }

wheel_page_up() {
  local n="${1:-10}" cx cy i
  activate; eval "$(window_geom)"
  cx=$((X + WIDTH / 2)); cy=$((Y + HEIGHT / 2))
  xdotool mousemove --sync "$cx" "$cy"; sleep 0.12
  xdotool keydown shift
  for i in $(seq 1 "$n"); do xdotool click 4; sleep 0.09; done
  xdotool keyup shift; release_mods
}

# Hover pane handle $1 (0-based, default 0) in the selected tab's strip. The
# handle row is native y 25..43 and handles are 15 px apart from x 10 (measured
# on the 1920x1080 box); hovering shows that pane's title in the title row.
hover_pane_handle() {
  local idx="${1:-0}"
  eval "$(window_geom)"
  xdotool mousemove --sync $((X + 17 + 15 * idx)) $((Y + 34)); sleep 1.6
  xdotool mousemove --sync $((X + WIDTH / 2)) $((Y + HEIGHT / 2))
}

detach_tty_attach() { release_mods; sleep 0.1; key ctrl+backslash; sleep 0.2; key d; }

rename_tab() {
  release_mods; sleep 0.15; key ctrl+shift+r; sleep 0.55
  xdotool type --delay 12 -- "$1"; sleep 0.25; enter; sleep 0.5
}

now() { python3 -c 'import time;print(time.time())'; }
elapsed() { python3 -c "import time;print(time.time()-${REC_START:-$(now)})"; }
# Wait until the clip for beat i has finished playing (plus its pause).
wait_clip() {
  local i="$1" end
  end=$(awk -v m="${MARK[$i]}" -v d="${DUR[$i]}" -v g="${PAUSE[$i]}" 'BEGIN{printf "%.3f", m+d+g}')
  python3 -c "import time,sys; t=float(sys.argv[1])+$REC_START-time.time(); time.sleep(max(0.0,t))" "$end"
}
# Beat i starts now: remember the timestamp for the narration mix.
MARK=()
beat() {
  local i="$1"
  MARK[$i]="$(elapsed)"
  log "beat $i: ${LINES[$i]:0:60}"
  printf '%s\t%s\t%s\t%s\n' "$i" "${MARK[$i]}" "${DUR[$i]}" "${LINES[$i]}" >> "$HOME/Desktop/demo-beats.tsv"
}

# Per-agent mailbox depth: `pmux mail --as AGENT inbox` prints "open: N held: M".
# Count both: the recipient's doorbell claims a letter (open -> held) the moment
# it lands, and holds it while the agent reads it, so "open" alone is a race.
mail_depth() { pmux mail --as "$1" inbox 2>/dev/null | python3 -c 'import re,sys; m=re.findall(r"\d+", sys.stdin.read()); print(sum(int(x) for x in m[:2]) if m else 0)'; }
mail_open() { pmux mail --as "$1" inbox 2>/dev/null | grep -Eo 'open: *[0-9]+' | grep -Eo '[0-9]+' | head -1 || echo 0; }
# Wait until AGENT has at least WANT letters (open or held), or the timeout passes.
wait_mail_open() {
  local agent="$1" want="${2:-1}" timeout="${3:-90}" t0 n
  t0=$(now)
  while :; do
    n=$(mail_depth "$agent"); n=${n:-0}
    [[ "$n" -ge "$want" ]] && return 0
    python3 -c "import time,sys; sys.exit(0 if time.time()-float(sys.argv[1])>=$timeout else 1)" "$t0" && return 1
    sleep 0.3
  done
}
# Wait until AGENT has no open letters (claimed) or the timeout passes.
wait_mail_drained() {
  local agent="$1" timeout="${2:-90}" t0 n
  t0=$(now)
  while :; do
    n=$(mail_open "$agent"); n=${n:-0}
    [[ "$n" -eq 0 ]] && return 0
    python3 -c "import time,sys; sys.exit(0 if time.time()-float(sys.argv[1])>=$timeout else 1)" "$t0" && return 1
    sleep 0.3
  done
}

# Wait until SESSION's pane revision has not changed for 2.5 s (output quiet).
wait_pane_quiet() {
  local session="$1" timeout="${2:-30}" t0 last cur stable
  t0=$(now); last=""; stable=0
  while :; do
    cur=$(pmux ls 2>/dev/null | awk -v s="session $session " 'index($0,s)==1{f=1;next} f&&/rev /{print $NF; exit}')
    if [[ "$cur" == "$last" ]]; then stable=$((stable + 1)); else stable=0; last="$cur"; fi
    [[ "$stable" -ge 5 ]] && return 0
    python3 -c "import time,sys; sys.exit(0 if time.time()-float(sys.argv[1])>=$timeout else 1)" "$t0" && return 1
    sleep 0.5
  done
}
# Wait until AGENT has no letters at all (claimed and committed).
wait_mail_committed() {
  local agent="$1" timeout="${2:-60}" t0 n
  t0=$(now)
  while :; do
    n=$(mail_depth "$agent"); n=${n:-0}
    [[ "$n" -eq 0 ]] && return 0
    python3 -c "import time,sys; sys.exit(0 if time.time()-float(sys.argv[1])>=$timeout else 1)" "$t0" && return 1
    sleep 0.3
  done
}
# Background watcher: touch FLAG the first time AGENT's mailbox depth > 0.
RANG_FLAG=/tmp/demo-rang
WATCH_PID=""
depth_watcher() {
  local agent="$1" flag="$2" n
  while :; do
    n=$(mail_depth "$agent"); n=${n:-0}
    [[ "$n" -ge 1 ]] && { touch "$flag"; return 0; }
    sleep 0.2
  done
}
wait_flag() {
  local flag="$1" timeout="${2:-90}" t0
  t0=$(now)
  while [[ ! -f "$flag" ]]; do
    python3 -c "import time,sys; sys.exit(0 if time.time()-float(sys.argv[1])>=$timeout else 1)" "$t0" && return 1
    sleep 0.2
  done
  return 0
}

case "${1:-}" in
  --check) check_setup; exit $? ;;
  --launch) launch_host; echo "window $(wait_for_window)"; size_host; exit 0 ;;
  --dry) DRY=1 ;;
  --clips) CLIPS_ONLY=1 ;;
  "") ;;
  *) echo "usage: $0 [--check|--launch|--dry|--clips]" >&2; exit 2 ;;
esac

[[ "${CLIPS_ONLY:-0}" -eq 1 ]] || check_setup
mkdir -p "$NARRDIR" "$HOME/Desktop"


N=${#LINES[@]}
DUR=()
if [[ "$DRY" -eq 0 ]]; then
  [[ -f "$ENV_FILE" ]] || { echo "ERROR: $ENV_FILE not found (use --dry for a silent take)" >&2; exit 1; }
  PRESET_VOICE_ID="${ELEVEN_VOICE_ID:-}"
  # shellcheck disable=SC1090
  source "$ENV_FILE"
  [[ -n "$PRESET_VOICE_ID" ]] && ELEVEN_VOICE_ID="$PRESET_VOICE_ID"
  : "${ELEVENLABS_API_KEY:?ELEVENLABS_API_KEY not set in $ENV_FILE}"
  VOICE_ID="${ELEVEN_VOICE_ID:-}"
  if [[ -z "$VOICE_ID" ]]; then
    VOICE_ID=$(curl -sS -H "xi-api-key: $ELEVENLABS_API_KEY" "https://api.elevenlabs.io/v1/voices" \
      | python3 -c "import sys,json; d=json.load(sys.stdin); print(next((v['voice_id'] for v in d.get('voices',[]) if v['name'].lower().startswith('$VOICE_NAME'.lower())), ''))" 2>/dev/null)
  fi
  [[ -n "$VOICE_ID" ]] || { echo "ERROR: no ElevenLabs voice id (set ELEVEN_VOICE_ID)" >&2; exit 1; }
  echo "  voice_id=$VOICE_ID model=$ELEVEN_MODEL speed=$ELEVEN_SPEED"
  tts() {
    local text="$1" out="$2" payload
    payload=$(python3 -c "import json,sys; print(json.dumps({'text':sys.argv[1],'model_id':sys.argv[2],'voice_settings':{'stability':0.5,'similarity_boost':0.75,'style':0.0,'use_speaker_boost':True,'speed':float(sys.argv[3])}}))" "$text" "$ELEVEN_MODEL" "$ELEVEN_SPEED")
    curl --fail-with-body --connect-timeout 15 --max-time 120 -sS -X POST "https://api.elevenlabs.io/v1/text-to-speech/$VOICE_ID" -H "xi-api-key: $ELEVENLABS_API_KEY" \
      -H "Content-Type: application/json" -H "Accept: audio/mpeg" -d "$payload" -o "$out"
    [[ "$(file --mime-type -b "$out")" == "audio/mpeg" ]] || { echo "ERROR: ElevenLabs did not return audio for: $text" >&2; head -c 300 "$out" >&2; exit 1; }
  }
  echo "Generate or reuse narration clips."
  for i in "${!LINES[@]}"; do
    raw="$NARRDIR/n${i}_${VOICE_ID:0:6}_s${ELEVEN_SPEED}.mp3"
    if [[ "${FORCE_TTS:-}" == 1 || ! -f "$raw" ]] || ! python3 -c 'import pathlib,sys; p=pathlib.Path(sys.argv[1]); sys.exit(0 if p.is_file() and p.read_text()==sys.argv[2] else 1)' "${raw}.txt" "${LINES[$i]}"; then
        tts "${LINES[$i]}" "$raw"
        printf '%s' "${LINES[$i]}" > "${raw}.txt"
    fi
    DUR+=("$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$raw")")
  done
  if [[ "${CLIPS_ONLY:-0}" -eq 1 ]]; then
    total=0
    for i in "${!LINES[@]}"; do
      printf '  clip %2d  %5.1fs  %s\n' "$i" "${DUR[$i]}" "${LINES[$i]:0:70}"
      total=$(awk -v t="$total" -v d="${DUR[$i]}" -v g="${PAUSE[$i]}" 'BEGIN{printf "%.1f", t+d+g}')
    done
    echo "Clips ready in $NARRDIR (spoken + pauses ≈ ${total}s). No recording in --clips mode."
    exit 0
  fi
else
  DUR=("${DRYDUR[@]}")
fi

# Fresh host for a clean opening. Keep the previous finished take until mixing succeeds.
rm -f "$FULL"
leftover="$(find_host_window)"
[[ -n "$leftover" ]] && { xdotool windowkill "$leftover" 2>/dev/null || true; sleep 0.4; }
rm -f "$HOME/.local/share/prismattyc/spaces/demo.json" 2>/dev/null || true
launch_host
wait_for_window >/dev/null
sleep 0.8
size_host
# Let the host refit its grid to the maximized window before the first frame.
sleep 2.0
activate

read -r SW SH < <(xdotool getdisplaygeometry)
SW=$((SW / 2 * 2)); SH=$((SH / 2 * 2))
echo "Start display recording ${SW}x${SH}."
ffmpeg -y -loglevel error -f x11grab -draw_mouse 0 -framerate 30 -video_size "${SW}x${SH}" -i "$DISPLAY" \
  -c:v libx264 -threads 2 -preset veryfast -pix_fmt yuv420p "$FULL" &
REC_PID=$!
REC_START=$(now)
: > "$HOME/Desktop/demo-beats.tsv"
sleep 1.5
kill -0 "$REC_PID" 2>/dev/null || { echo "ERROR: ffmpeg died" >&2; exit 1; }

# ---- 0 splash / intro ------------------------------------------------------
beat 0
sleep 5.0
activate; enter
sleep 1.0
activate; type_cmd "cd '$PARTS' && clear && ./ansi.sh"
wait_clip 0

# ---- 1 footer hold ---------------------------------------------------------
beat 1
activate
hold_ctrl_shift
python3 -c "import time; time.sleep(${DUR[1]})"
release_ctrl_shift
wait_clip 1

# ---- 2 command palette -> theme settings -----------------------------------
beat 2
activate; key ctrl+shift+p; sleep 1.2
xdotool type --delay 40 -- "theme"; sleep 1.0
enter; sleep 1.0
wait_clip 2

# ---- 3 themes --------------------------------------------------------------
beat 3
activate
key Down; sleep 1.1; key Down; sleep 1.1; key Down; sleep 1.1
key Return; sleep 0.8
activate; type_cmd "clear"
wait_clip 3

# ---- 4 truecolor + unicode -------------------------------------------------
beat 4
activate; type_cmd "./colors.sh"; sleep 1.2
activate; type_cmd "./unicode.sh"
wait_clip 4

# ---- 5 cell sprites --------------------------------------------------------
beat 5
activate; type_cmd "clear && ./box.sh"
wait_clip 5

# ---- 6 kitty graphics ------------------------------------------------------
beat 6
activate; type_cmd "./image.sh"
wait_clip 6

# ---- 7 scrollback + find ---------------------------------------------------
beat 7
activate; type_cmd "seq 1 400"; sleep 0.8
wheel_page_up 20; sleep 0.8
activate; key ctrl+shift+f; sleep 0.8
xdotool type --delay 60 -- "42"; sleep 1.2
key Return; sleep 1.0
key Escape; sleep 0.3
activate; type_cmd "clear"
wait_clip 7

# ---- 8 panes, quadrants, zoom ---------------------------------------------
beat 8
activate; key ctrl+shift+backslash; sleep 1.2
key alt+Right; sleep 0.5
activate; type_cmd "date"; sleep 0.6
key alt+Left; sleep 0.5
key ctrl+alt+4; sleep 1.8
key alt+Right; sleep 0.5; key alt+Down; sleep 0.5
key ctrl+shift+z; sleep 1.6
key ctrl+shift+z; sleep 0.8
wait_clip 8

# ---- 9 tabs + rename + pane handles ----------------------------------------
beat 9
activate; key ctrl+shift+t; sleep 0.8
rename_tab "build"
activate; type_cmd "echo build tab"; sleep 0.3
key ctrl+shift+t; sleep 0.8
rename_tab "logs"
activate; type_cmd "echo logs tab"; sleep 0.3
key ctrl+shift+1; sleep 0.6
rename_tab "grid"
hover_pane_handle
key ctrl+shift+Next; sleep 0.6; key ctrl+shift+Next; sleep 0.6
wait_clip 9

# ---- 10 rename a pane (palette rename_pane; the handle shows the name) ----
beat 10
activate; key ctrl+shift+1; sleep 0.6
activate; key ctrl+shift+p; sleep 1.0
xdotool type --delay 40 -- "renamepane"; sleep 0.8   # no shifted chars: the palette filter drops shift (PT ticket pending)
enter; sleep 0.8
xdotool type --delay 50 -- "editor"; sleep 0.6
enter; sleep 0.6
hover_pane_handle 3
wait_clip 10

# ---- 11 busy pane: the handle breathes while the pane produces output ----
beat 11
activate; key alt+Right; sleep 0.4
activate; type_cmd "for i in \$(seq 1 80); do echo \"working step \$i\"; sleep 0.12; done"
sleep 0.3
key alt+Left; sleep 0.5
hover_pane_handle 3
wait_clip 11

# ---- 12 mux sessions -------------------------------------------------------
beat 12
activate; type_cmd "clear && pmux ls"
wait_clip 12

# ---- 13 attach / detach / reattach -----------------------------------------
beat 13
activate; type_cmd "clear && pmux attach work"; sleep 1.8
activate; type_cmd "echo 'pmux keeps this'"; sleep 0.4
activate; type_cmd "seq 1 10"; sleep 1.5
activate; detach_tty_attach; sleep 1.5
activate; type_cmd "pmux attach work"; sleep 2.0
activate; detach_tty_attach; sleep 0.6
wait_clip 13

# ---- 14 spaces: save the arrangement; the rail chip appears -------------
beat 14
activate; key ctrl+shift+3; sleep 0.5
activate; type_cmd "clear && pmux space save demo"; sleep 2.5
activate; type_cmd "pmux space ls"; sleep 1.5
activate; key ctrl+shift+p; sleep 0.6
xdotool type --delay 40 -- "spacesettings"; enter; sleep 3
wait_clip 14
key Escape

# ---- 15 agents: two more tabs, each attached to an agent seat --------------
beat 15
activate; key ctrl+shift+t; sleep 0.8
rename_tab "claude-view"
activate; type_cmd "pmux attach claude"; sleep 2.0
activate; type_cmd "claude"; sleep 1.0
key ctrl+shift+t; sleep 0.8
rename_tab "kiro-view"
activate; type_cmd "pmux attach kiro"; sleep 2.0
activate; type_cmd "kiro-cli chat --agent pmux --trust-tools @pmux"; sleep 1.0
key ctrl+shift+4
# Both TUIs need a moment to load their MCP servers; the narration covers most of it.
sleep 6
wait_clip 15

# ---- 16 Claude writes Kiro -------------------------------------------------
beat 16
activate; key ctrl+shift+4; sleep 0.5
activate
type_str 'Use the pmux_send tool once: to "kiro", summary "hello from claude", body "Kiro, in one sentence, what can you see from your seat? Reply with pmux_send to agent id claude." Then say "sent" and stop.'
sleep 0.4; enter
wait_mail_open kiro 1 120 || { log "no letter for kiro within 120 s"; [[ "$DRY" -eq 1 ]] || { echo "ERROR: agent beat failed; capture kept at $FULL" >&2; exit 1; }; }
# From here on, watch Claude's mailbox: Kiro's reply can land (and be claimed)
# while the next clip is still playing.
rm -f "$RANG_FLAG"; depth_watcher claude "$RANG_FLAG" & WATCH_PID=$!
sleep 1.0
wait_clip 16

# ---- 17 doorbell in Kiro's pane -------------------------------------------
beat 17
# Switch at once: the doorbell claims the letter within about a second of
# it landing, so any hold here loses the envelope. The letter cell is on
# screen right after the switch; remember this moment and the mix freezes
# the frame here for FREEZE_S seconds with a callout so viewers can see it.
activate; key ctrl+shift+5; sleep 0.3
ENV_T="$(elapsed)"; sleep 1.7
# The doorbell injects PMUX_MAIL and submits it. Only if the letter is still
# unclaimed after a few seconds, press Enter to submit the composer by hand.
wait_mail_drained kiro 8 || { activate; enter; }
wait_clip 17

# ---- 18 Kiro answers: show the reply going out from Kiro's pane -------------
beat 18
# Kiro claims (depth drops), replies (depth rises for claude), commits.
wait_mail_drained kiro 120 || log "warn: kiro did not claim within 120 s"
wait_flag "$RANG_FLAG" 120 || { log "no reply for claude within 120 s"; [[ "$DRY" -eq 1 ]] || { echo "ERROR: agent beat failed; capture kept at $FULL" >&2; exit 1; }; }
log "claude's mailbox rang"
activate; key ctrl+shift+5; sleep 1.0
wait_clip 18

# ---- 19 Claude receives: the doorbell rings and the answer is on screen ------
beat 19
activate; key ctrl+shift+4; sleep 2.0
mail_diag() {
  local tag="$1" mux_log
  mux_log="$(pmux status 2>/dev/null | awk '/^log:/{print $2}')"
  {
    echo "== mail diag ($tag) $(elapsed)"
    pmux mail --as claude inbox 2>&1 | head -3
    pmux doctor claude 2>&1 | head -6
    pmux clients 2>&1 | head -6
    if [[ -n "$mux_log" ]]; then grep -iE 'inject|doorbell|nudge' "$mux_log" | tail -25; fi
    true
  } >> "$HOME/Desktop/demo-mail-diag.log" 2>&1 || true
}
: > "$HOME/Desktop/demo-mail-diag.log"; mail_diag before
# The doorbell may have fired while Kiro's tab was up, or it may still be
# deferred. Wait for Claude to claim (open -> 0) and commit (depth -> 0);
# never press Enter into an idle composer.
# Wait for the claim. If the doorbell has not landed in 10 s, ring it again
# with the product's own verb (`pmux mail SESSION`), which also reports the
# inject outcome, and keep the outcome in the diag log.
ring=0
until wait_mail_drained claude 10; do
  ring=$((ring + 1))
  [[ "$ring" -gt 6 ]] && { log "warn: claude did not claim after 6 rings"; mail_diag "not claimed"; break; }
  out="$(pmux mail claude 2>&1 || true)"; log "ring $ring: $out"
  echo "== ring $ring $(elapsed): $out" >> "$HOME/Desktop/demo-mail-diag.log"
done
wait_mail_committed claude 60 || log "warn: claude did not commit within 60 s"
mail_diag after
# Claude commits before it finishes writing its summary; wait for the pane
# to go quiet so the reply text is on screen, then hold on it.
wait_pane_quiet claude 30 || log "warn: claude pane still busy after 30 s"
sleep 4
wait_clip 19

# ---- 20 accessibility: the tree (config lines + palette on screen) -----------
beat 20
activate; key ctrl+shift+t; sleep 0.8
rename_tab "a11y"
activate; type_cmd "clear && grep -B1 -A4 '^\\[a11y\\]' ~/.config/prismattyc/config.toml"
sleep 1.0
activate; key ctrl+shift+p; sleep 1.2
xdotool type --delay 40 -- "palette"; sleep 2.0
key Escape; sleep 0.5
wait_clip 20

# ---- 21 accessibility: announcements (help lists the a11y keys) -------------
beat 21
activate; type_cmd "prismattyc-host --help 2>&1 | grep -A3 'a11y'"
wait_clip 21

# ---- 22 intentional pane writes: execute, inspect, remove and kill -----------
beat 22
activate; key ctrl+shift+1; sleep 0.8
key ctrl+shift+z; sleep 0.6
rm -f "$HOME/Desktop/pane-write-result.txt"
activate; type_cmd "clear && PRISMATTYC_DEMO_RESULT=\"\$HOME/Desktop/pane-write-result.txt\" python3 '$PARTS/pane-messaging.py'"
wait_clip 22
grep -qx PASS "$HOME/Desktop/pane-write-result.txt" || { echo "ERROR: pane-write demo did not finish" >&2; exit 1; }

# ---- 23 outro --------------------------------------------------------------
beat 23
activate; key ctrl+shift+t; sleep 0.8
activate; type_cmd "prismattyc"
wait_clip 23
sleep 1.0

echo "Stop recording."
kill -INT "$REC_PID" 2>/dev/null || true; wait "$REC_PID" 2>/dev/null || true; REC_PID=""
sleep 1

if [[ "$DRY" -eq 1 ]]; then
  mv "$FULL" "$OUT"
  printf 'beat marks:'; for i in "${!MARK[@]}"; do printf ' %d=%.1f' "$i" "${MARK[$i]}"; done; echo
  echo "Done (silent): $OUT"
  exit 0
fi

# Freeze the envelope moment (beat 17) for FREEZE_S seconds with a callout,
# and shift every later beat mark by the same amount.
FREEZE_S=3
if [[ -n "${ENV_T:-}" && "$DRY" -eq 0 ]]; then
  echo "Freeze ${FREEZE_S}s at ${ENV_T}s (envelope callout)."
  RAW_CAPTURE="$FULL"; FROZEN="${FULL%.mp4}_frozen.mp4"
  FONT="$(fc-match -f '%{file}' 'sans-serif:style=Bold')"
  ffmpeg -y -loglevel error -i "$FULL" -filter_complex_threads 2 -filter_complex \
    "[0:v]trim=0:${ENV_T},setpts=PTS-STARTPTS,tpad=stop_mode=clone:stop_duration=${FREEZE_S}[v0];[0:v]trim=start=${ENV_T},setpts=PTS-STARTPTS[v1];[v0][v1]concat=n=2:v=1:a=0[vc];[vc]drawtext=fontfile=${FONT}:text='A new message for Kiro':fontsize=36:fontcolor=#f0b050:box=1:boxcolor=#1a1a2a@0.85:boxborderw=8:x=(w-tw)/2:y=64:enable='between(t,${ENV_T},${ENV_T}+${FREEZE_S})'[v]" \
    -map "[v]" -an -c:v libx264 -threads 2 -preset medium -crf 18 -pix_fmt yuv420p "$FROZEN"
  FULL="$FROZEN"
  rm -f "$RAW_CAPTURE"
  # Marks after the freeze shift by FREEZE_S. A clip that would straddle the
  # freeze (beat 17: "Watch Kiro's pane") starts at the freeze instead, so it
  # plays over the held frame rather than being cut in two by silence.
  for i in "${!MARK[@]}"; do
    MARK[$i]=$(awk -v m="${MARK[$i]}" -v d="${DUR[$i]}" -v t="$ENV_T" -v f="$FREEZE_S" \
      'BEGIN{ if (m>t) printf "%.3f", m+f; else if (m+d>t) printf "%.3f", t; else printf "%.3f", m }')
  done
fi
# Retain final timestamps after the callout pause for captions and review.
: > "$HOME/Desktop/demo-beats-final.tsv"
for i in "${!LINES[@]}"; do
  printf '%s\t%s\t%s\t%s\n' "$i" "${MARK[$i]}" "${DUR[$i]}" "${LINES[$i]}" >> "$HOME/Desktop/demo-beats-final.tsv"
done
echo "Mix narration at the recorded beat marks."
INPUTS=(-i "$FULL"); FILTER=""; MIX=""
for i in "${!LINES[@]}"; do
  INPUTS+=(-i "$NARRDIR/n${i}_${VOICE_ID:0:6}_s${ELEVEN_SPEED}.mp3")
  ms=$(awk -v m="${MARK[$i]}" 'BEGIN{printf "%d", m*1000}')
  FILTER+="[$((i+1)):a]aresample=44100,aformat=channel_layouts=stereo,adelay=${ms}|${ms},apad[a$i];"
  MIX+="[a$i]"
done
FILTER+="${MIX}amix=inputs=$N:duration=longest:dropout_transition=0:normalize=0[a]"
MIXED="${OUT%.mp4}.partial.mp4"
ffmpeg -y -loglevel error "${INPUTS[@]}" -filter_complex_threads 2 -filter_complex "$FILTER" \
  -map 0:v -map "[a]" -c:v copy -c:a aac -b:a 160k -movflags +faststart -shortest "$MIXED"
vdur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$FULL")
odur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$MIXED")
echo "  video ${vdur}s -> output ${odur}s"
awk -v v="$vdur" -v o="$odur" 'BEGIN{exit !(o+1.0 >= v)}' || { echo "ERROR: output shorter than the capture; raw kept at $FULL" >&2; exit 1; }
mv -f "$MIXED" "$OUT"
rm -f "$FULL"
printf 'beat marks:'; for i in "${!MARK[@]}"; do printf ' %d=%.1f' "$i" "${MARK[$i]}"; done; echo
cp -f "$HOST_LOG" "$HOME/Desktop/demo-host.log" 2>/dev/null || true
mux_log="$(pmux status 2>/dev/null | awk '/^log:/{print $2}')"; [[ -n "$mux_log" ]] && cp -f "$mux_log" "$HOME/Desktop/demo-pmux.log" 2>/dev/null || true
echo "Done: $OUT"
