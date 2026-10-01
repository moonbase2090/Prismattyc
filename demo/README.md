# Record the narrated demo reel

`record-demo.sh` drives Prismattyc in a private Xvfb display and records the
screen at 1920×1080. The recorder places one narration clip at each beat's
recorded start time. Silent mode uses fixed beat lengths and does not call
ElevenLabs.

The Docker runner builds on the public image from `tests/native/docker`. It
limits each recorder container to two CPUs and 8 GiB of memory. The display
uses the generic `demo` account and the `prismattyc` hostname.

## Build and check the box

Build the native test image and the demo image, then run the setup check:

```bash
demo/docker/run.sh build
demo/docker/run.sh --check
```

The runner builds Prismattyc's Linux binaries inside a two-CPU container. It
installs the Codex and Muse CLIs in the derived demo image. `--check` verifies
the binaries, tools, Xvfb display, and staged credential copies. It does not
start a recording or call ElevenLabs.

## Record a take

Run one silent take before a narrated take:

```bash
demo/docker/run.sh --dry
demo/docker/run.sh
```

`--dry` records the screen without narration or ElevenLabs calls. The runner
places each take under `build/demo-reel-dry/<run-id>/demo-reel.mp4`.
Narrated mode writes to `build/demo-reel/<run-id>/demo-reel.mp4`.

For narration, copy `demo/.eleven.env.example` to `demo/.eleven.env` and set
`ELEVENLABS_API_KEY`. The recorder uses the Daniel voice by default. Set
`ELEVEN_VOICE_ID` to pin another voice or `ELEVEN_SPEED` to change the pace.
Use `demo/docker/run.sh --clips` to make or reuse narration clips without
capturing the display.

The recorder keeps the previous output until `ffprobe` confirms that the
silent take or narration mix is at least as long as the raw capture. If the
check fails, it keeps the raw file in the run directory.

## Isolate the agents

The runner copies Codex and Muse authentication files into a private local
cache, then mounts those copies into the disposable container. It never mounts
the host credential directories. The recorder copies them again into a
per-take `CODEX_HOME` and XDG config directory.

`--check` confirms that the credential copies exist; it does not sign in to
either service. Muse's Linux process needs an auth file the installed CLI can
read without the host's OS credential store. If the default Muse file refers
to a host-only credential store, set `PRISMATTYC_MUSE_AUTH_FILE` to a private,
Linux-compatible auth file. A silent take can continue when a mailbox scene
cannot complete; narrated mode fails instead of presenting an incomplete
exchange as finished.

Each take creates its own pmux socket under a private runtime directory. The
work, Codex, and Muse sessions use that socket. The app and both agents inherit
it, and recorder commands pass it explicitly. The Docker container has no
mount for the host pmux socket.

Codex starts with its local shell features disabled. Its pmux MCP entry is
limited to the tutorial and mailbox tools needed by the reel, with those
isolated tools approved automatically. Muse runs in its sandbox inside the
disposable container. The mounted source tree is read-only, and the agent
sessions and their MCP settings are created fresh for each take.
Every pmux command and MCP connection uses the unique per-take socket. Agent
mail stays between the two demo sessions.

The reel covers Ctrl+Enter delivery, configurable key bindings, active
bindings from `prismattyc-host --list-bindings`, terminal rendering,
scrollback search, tabs, panes, Codex and Muse mail, and accessibility. It
keeps two separate collaboration scenes: a `seq 1 10000` write to an isolated
shell pane, then a prompt written to an agent pane with a pane-write receipt.
The prompt text and literal carriage return are sent separately. Four seconds
after the first carriage return, the recorder sends one more only if the
prompt remains in the Codex or Muse composer. A delayed agent reply does not
trigger a second Enter; the take waits until the reply is visible. Codex is the
default target for the reply beat; set `PRISMATTYC_DEMO_PANE_AGENT=muse` to
target Muse instead.

`.eleven.env` is ignored by Git. Credential copies and generated recordings
stay outside the source tree.
