# Remote localhost port-forward and file drop

Status: **proposal from a time-boxed spike, not accepted direction.** Nothing here
changes shipped behavior.

Base: `origin/main` `3bba8e8f`. Line references are to that commit.

## Summary

Today Prismattyc opens remote Spaces as local PTY tabs running `ssh -t` into
`pmux attach` (`crates/prismattyc-host/src/main.rs:10144-10163`). Clicking a
`http://localhost:3000` link in that tab opens the user's **local** port 3000
instead of forwarding from the remote machine, and the host does not handle OS
file drag-and-drop events at all.

This note splits the work into two independent builds that share one small
host-side prerequisite (`RemotePaneBinding` and a `RemoteTransport` seam):

1. **Slice A — Localhost port-forward:** clicking a localhost URL in a known
   remote pane prompts for explicit confirmation, starts a loopback-only
   host-managed tunnel (`127.0.0.1`), and opens the local browser; a palette
   command and forwards table let users create, inspect, and stop forwards.
2. **Slice B — Remote file drop:** dropping local file(s) onto a known remote
   pane shows a consent modal, streams the bytes over `ssh -T` into a new
   shell-free `pmux drop-receive` endpoint (`0700` dir, `O_EXCL` `0600` files,
   automatic TTL + session cleanup), and inserts the sanitized remote path via
   `PasteJobs` bracketed paste (or copies it to the clipboard).

**Recommendation: go on both slices**, built separately:

- **Slice A (port-forward):** **10–12 agent-hours / 2.5–3 wall-clock days**
  (2 PRs).
- **Slice B (remote file drop):** **8–10 agent-hours / 2.5–3 wall-clock days**
  (2 PRs).
- **Combined:** **18–22 agent-hours / 5–6 wall-clock days** (4 PRs total,
  including real-SSH integration tests and Windows VM proof).

## What Prismattyc has today

- **Remote Spaces over SSH (`docs/design/remote-spaces-ssh.md`,
  `crates/prismattyc-mux/src/remote_catalog.rs`,
  `crates/prismattyc-host/src/remote_catalog.rs`):**
  - Users configure `[[remote]]` entries with validated `DestinationId` and
    `SshAlias` (never starting with `-`; `remote_catalog.rs:35-118`).
  - Non-PTY catalog calls run `ssh -T -o BatchMode=yes -o ConnectTimeout=10 -- ALIAS pmux space catalog`
    via `prismattyc_mux::platform::hidden_command("ssh")`
    (`crates/prismattyc-host/src/remote_catalog.rs:42-53`).
  - `open_remote_attach` (`crates/prismattyc-host/src/main.rs:10144-10163`)
    spawns a new tab running `ssh -t -o BatchMode=yes -o ConnectTimeout=10 -- ALIAS pmux attach --session-id N --space-id HEX`
    (`crates/prismattyc-host/src/remote_catalog.rs:60-81`) and sets the tab
    title, **but does not store the typed `DestinationId` / `SshAlias` on the
    pane**.
- **Link detection and opening (`crates/prismattyc-host/src/hyperlink.rs`,
  `link_click.rs`):**
  - `hyperlink::target_at` (`hyperlink.rs:149-198`) resolves explicit OSC 8
    links and auto-detected `http://`/`https://` runs, rejecting `javascript:`,
    `file:`, and `data:` (`hyperlink.rs:316-333`).
  - `HoverPreview` (`hyperlink.rs:29-95`) displays the parsed host before the
    full URI to defeat `@`-userinfo spoofing.
  - `link_click::Gesture::take_due` (`link_click.rs:206-218`) returns
    `Vec<String>`, dropping `pending.pane` before `main.rs:4217-4220` calls
    `open_url(&url)` (`hyperlink::spawn_open`, `hyperlink.rs:355-407`).
- **Confirmation modals and command palette:**
  - `restart_modal.rs` and `graphite_overlays::modal_panel` already provide a
    keyboard-, pointer-, and AccessKit-accessible confirmation modal pattern.
  - `palette.rs` hosts searchable actions (`PaletteEntry`) with descriptions
    and chords.
- **Drag-and-drop and paste:**
  - `winit` `0.30.13` exposes `WindowEvent::HoveredFile(PathBuf)`,
    `DroppedFile(PathBuf)`, and `HoveredFileCancelled`, but
    `crates/prismattyc-host/src/main.rs` currently handles **none** of them
    (`main.rs:21572-22636`).
  - `paste_job.rs` (`PasteJobs`, `main.rs:18861-19028`) delivers pastes
    asynchronously on the pane's writer thread with DECSET 2004 bracketed-paste
    framing (`\x1b[200~…\x1b[201~`) and never blocks the main thread.
  - `crates/prismattyc-mux/src/image_paste.rs` saves local clipboard images to
    `$XDG_RUNTIME_DIR/prism-paste/` and pastes the local path; that path is not
    reachable from a remote pane.

## Shared prerequisite: typed remote pane binding and transport seam

Neither port-forwarding nor file drop should ever guess whether a pane is
remote by scraping tab titles, prompts, or local shell command lines.

1. **`RemotePaneBinding` on `HostState`:** when `open_remote_attach`
   (`main.rs:10144`) creates a remote tab via `host.mux.new_tab`
   (`mux.rs:5135`), record a typed binding keyed by the new pane's identity:
   `RemotePaneBinding { destination: DestinationId, ssh_alias: SshAlias, label: String, space: RemoteSpaceId, session: RemoteSessionId }`.
   Remove the binding when that pane closes or its `ssh` child exits.
   - Manual `ssh` run inside a local shell stays a plain local pane (no
     implicit host-side SSH calls to unconfigured hosts).
2. **`RemoteTransport` seam (future-proofing identity-based remotes):**
   ```rust
   trait RemoteTransport {
       fn start_forward(&self, local_port: u16, remote_port: u16) -> Result<ForwardHandle>;
       fn upload_file(&self, meta: DropRequest, reader: Box<dyn Read + Send>) -> Result<RemotePath>;
   }
   ```
   - Today `SshTransport` implements both methods using the configured
     `SshDestination` and system `ssh`.
   - A future identity-based remote (such as a Tailscale-authenticated remote
     daemon with no SSH keys) plugs into the same `RemoteTransport` trait
     without changing URL detection, confirmation modals, the forwards table,
     drop consent, `PasteJobs` insertion, or `pmux drop-receive`.

---

## Slice A: Automatic localhost port-forward

### 1. Detecting remote localhost URLs

1. Change `link_click::Gesture::take_due` (`link_click.rs:206`) to return
   `Vec<(P, String)>` so `main.rs:4217` knows which pane the click originated
   in.
2. Parse the clicked URL with `url::Url::parse` (already a dependency of
   `prismattyc-host`). Classify it as a **localhost target** only when:
   - scheme is `http` or `https` (preserving `hyperlink::is_allowed_http_url`),
   - parsed host is one of:
     - `Host::Domain("localhost")` (case-insensitive),
     - `Host::Ipv4(ip)` where `ip.is_loopback()` (`127.0.0.0/8`) or
       `ip.is_unspecified()` (`0.0.0.0`, commonly printed by dev servers),
     - `Host::Ipv6(ip)` where `ip.is_loopback()` (`::1`) or
       `ip.is_unspecified()` (`::`),
   - resolved port (`url.port_or_known_default()`) is in `1..=65535`.
3. When the host was `0.0.0.0` or `::`, normalize the remote forward target to
   `127.0.0.1` (or `[::1]` for IPv6 loopback) — never forward to an unspecified
   address.
4. For **local panes** (no `RemotePaneBinding`), behavior is unchanged: open
   directly via `open_url(&url)`.

### 2. Where the forward lives and SSH transport

The forward **lives in the local desktop host (`prismattyc-host`)**, not in the
remote `pmuxd` server:

- Loopback listeners bind on the user's local machine (`127.0.0.1`), and the
  host already owns `[[remote]]` config, SSH credentials/agent integration, the
  confirmation UI, and `hyperlink::spawn_open`.
- Remote `pmuxd` needs zero new network-proxying code or attack surface.

**SSH invocation (per forward, cross-platform including Windows OpenSSH):**

```text
ssh -N -T \
  -o BatchMode=yes \
  -o ConnectTimeout=10 \
  -o ExitOnForwardFailure=yes \
  -o GatewayPorts=no \
  -o ServerAliveInterval=15 \
  -o ServerAliveCountMax=3 \
  -L 127.0.0.1:<local_port>:127.0.0.1:<remote_port> \
  -- <ssh_alias>
```

Why one `ssh -N` child per forward instead of `ControlMaster` / `ssh -O forward`:

- Windows OpenSSH does **not** support Unix-domain `ControlMaster` fd passing,
  whereas `ssh -N -T -o ExitOnForwardFailure=yes -L ...` works identically on
  macOS, Linux, and Windows 10/11 OpenSSH via `hidden_command("ssh")`.
- Each forward has an isolated lifecycle, clear exit status, and clean
  per-forward teardown (`kill` + `wait`) without affecting attached PTY tabs.
- (Optional optimization on Unix later: reuse a user-configured `ControlMaster`
  automatically if already set in `~/.ssh/config`, without requiring it.)

**Local port selection and browser launch:**

- Prefer `local_port == remote_port` if `TcpListener::bind(("127.0.0.1", remote_port))`
  succeeds (then drop the probe listener immediately before spawning `ssh`); if
  occupied, bind `("127.0.0.1", 0)` to pick a free ephemeral port and show the
  exact local port in the confirmation modal.
- Wait on a background worker (never the main thread) until the local port
  accepts a short TCP probe or `ssh` exits with stderr (bounded by the 10 s
  connect timeout) before calling `hyperlink::spawn_open` on the rewritten URL.
- When rewriting the URL to open locally, prefer `http(s)://localhost:<local_port><path><query><fragment>`
  if the clicked host was `localhost` (preserving browser secure-context APIs
  such as WebCrypto and Service Workers on `localhost`), or
  `127.0.0.1:<local_port>` if the clicked host was an IP literal.

### 3. Confirmation, trust boundary, and authorization

**Every new forward requires explicit user confirmation.** Remote terminal
output is untrusted; neither an OSC escape sequence nor a plain link click may
silently open a local listening socket.

- **Click on an already-forwarded `(destination, remote_port)`:** no duplicate
  tunnel is started; open the existing local URL directly.
- **Click on a new `(destination, remote_port)`:** open a host confirmation
  modal (following `restart_modal.rs`) displaying:
  - Remote destination label and SSH alias,
  - Remote target (`127.0.0.1:<remote_port>`) and local bind
    (`127.0.0.1:<local_port>`),
  - Exact local URL that will open in the browser,
  - Explicit warning if an OSC 8 link's visible text does not match its
    parsed host/port,
  - Choices: **Forward and open**, **Open local URL without forwarding**,
    **Cancel** (`Esc` cancels).
- **Hard security invariants:**
  - Always pass `-o GatewayPorts=no` and bind `127.0.0.1` explicitly so local
    forwards are never exposed to the LAN, even if `~/.ssh/config` sets
    `GatewayPorts yes`.
  - Always pass `-o BatchMode=yes` and `-- <ssh_alias>` (no hidden password
    prompts, no option injection).
  - Never allow forwarding to arbitrary non-loopback remote hosts on link click
    (remote side is strictly `127.0.0.1` or `[::1]`; remote-LAN jump forwarding
    is out of scope).
  - Cap concurrent forwards (default max **16** per host, **8** per
    destination).

### 4. Lifetime and forwards table UI

- **Lifetime:**
  - Active forwards are owned by the host process (`ForwardManager` on `App`,
    keyed by `(DestinationId, remote_port)`).
  - A forward stays alive while at least one remote tab to that `DestinationId`
    is open, and is **automatically stopped** when the last tab to that
    destination closes (preventing forgotten background listeners) or when the
    host exits.
  - Users can stop any forward (or stop all) at any time from the forwards
    table.
- **UI surfaces:**
  - **Command palette (`palette.rs`):**
    - `Forward Port…` — prompts for destination (defaulting to the focused
      remote pane's destination), remote port, and optional local port.
    - `Port Forwards` — opens the active forwards overlay table.
  - **Forwards table overlay:** lists `Destination | Local | Remote | Status | Open | Stop`,
    keyboard- and pointer-navigable with AccessKit rows, plus a subtle chrome
    indicator when forwards are active so open tunnels remain visible.

---

## Slice B: Remote file drop

### 1. Host drop handling and `winit` constraints

- Wire `WindowEvent::HoveredFile`, `WindowEvent::HoveredFileCancelled`, and
  `WindowEvent::DroppedFile` in `crates/prismattyc-host/src/main.rs`.
- **Important `winit 0.30.13` behavior:** `DroppedFile(PathBuf)` fires once per
  file in a multi-file drop and carries **no pointer coordinates** (and on
  macOS/Windows `CursorMoved` may not fire during an OS file drag).
  - Batch contiguous `DroppedFile` events received in the same event-loop turn
    into one drop batch (default cap: **10 files**, **64 MiB** per file,
    **128 MiB** per batch).
  - Target the **currently focused pane** (and show the exact pane and
    destination name in the consent modal so split-pane targets are never
    ambiguous).
- While `HoveredFile` is active over a remote pane, show a lightweight drop
  overlay ("Drop to upload to `<destination>`") so the user sees where the file
  will go before releasing.
- Local panes are left unchanged in v1 (or may optionally paste local paths in
  a separate local-only enhancement; never upload a local drop).

### 2. Pre-upload validation and explicit consent

Never upload a local file silently on an accidental drag-drop.

**Pre-consent checks (run off the main thread for metadata, fail closed):**

1. Require regular files only in v1 (`symlink_metadata` + reject FIFOs,
   sockets, device nodes, and directories with a clear toast).
2. Reject symlinks by default (or flag them explicitly and refuse symlinks to
   non-regular targets) so dropping a symlink cannot silently exfiltrate an
   unexpected file.
3. Enforce size caps (default 64 MiB/file, 128 MiB/batch; configurable).
4. On Windows, reject UNC/network paths (`\\server\share\…`, `\\?\UNC\…`),
   device paths (`\\.\…`), and `.lnk` shortcuts / reparse points
   (`FILE_ATTRIBUTE_REPARSE_POINT`) to prevent NTLM credential leaks or SMB
   hangs.

**Consent modal (following `restart_modal.rs`):**

- Shows destination label and SSH alias (`Upload to devbox`), focused remote
  session/tab name, each file's basename and size (highlighting dotfiles such
  as `.env`), and the cleanup policy ("Stored in private temp dir; cleaned on
  session end / 24 h TTL").
- Actions:
  - **Upload and paste path** (default)
  - **Upload and copy path** (copies remote path(s) to local clipboard without
    writing to the PTY)
  - **Cancel** (`Esc` cancels; nothing touches the network)
- Shows a progress/cancel state while uploading on a background worker thread
  so the main thread never blocks.

### 3. Shell-free upload transport and secure remote temp cleanup

Do **not** use `scp` or construct remote shell commands (`ssh host "cat > ..."`).
Shell interpolation of filenames invites quoting bugs, world-readable temp
files, and `/tmp` symlink races.

Instead, add a dedicated, non-interactive CLI subcommand on `pmux` (mirroring
`pmux space catalog` in `crates/prismattyc-host/src/remote_catalog.rs:25`):

```text
ssh -T -o BatchMode=yes -o ConnectTimeout=10 -- <ssh_alias> pmux drop-receive
```

**Wire protocol on stdin/stdout (zero untrusted filenames on remote argv):**

1. Host writes a single bounded JSON header line (`<= 4 KiB`) to stdin:
   ```json
   {"version":1,"name":"screenshot.png","bytes":48210,"session_id":42}
   ```
   followed immediately by exactly `bytes` of raw file data, then closes stdin.
2. Remote `pmux drop-receive`:
   - Rejects unknown versions, `bytes > max_drop_bytes` (default 64 MiB), or
     malformed headers before creating any file.
   - Sanitizes `name` to a safe ASCII basename (`[A-Za-z0-9._-]`, stripping
     path separators, NULs, control chars, and leading `.`/`-`; max 64 chars,
     fallback `file`).
   - Ensures a per-user `0700` base directory under `$XDG_RUNTIME_DIR/prism-drops`
     (falling back to a verified `0700` per-user directory via `private_fs.rs`,
     never a shared world-writable `/tmp` flat directory), scoped under
     `s-<session_id>/` when `session_id` is supplied.
   - Creates the destination file with `O_CREAT | O_EXCL | O_WRONLY | O_NOFOLLOW`
     and mode **`0600`** using a random 64-bit hex prefix from `getrandom`
     (`<hex16>-<sanitized_name>`).
   - Copies **exactly** `bytes` from stdin (failing and unlinking the partial
     file on short read, extra trailing bytes, or I/O error), calls `sync_all`,
     and emits one JSON line to stdout:
     ```json
     {"version":1,"path":"/run/user/1000/prism-drops/s-42/9f4c2a1b8e3d7051-screenshot.png"}
     ```
3. If the remote `pmux` is older and does not recognize `drop-receive`, the host
   reports a clear error ("remote pmux does not support file drop; update pmux
   on `<destination>`") rather than falling back to an unsafe shell command.

**Cleanup policy (three layers):**

- **Automatic TTL and size cap:** every `pmux drop-receive` invocation (and
  `pmuxd` maintenance/startup) prunes drop files older than **24 hours** and
  enforces a **512 MiB** total directory cap (oldest first, matching the
  bounded retention pattern in `image_paste.rs:221-244`).
- **Session-destroy and reboot cleanup:** `pmuxd` removes `s-<session_id>/`
  when a session is destroyed; on Linux `$XDG_RUNTIME_DIR` is `tmpfs` and
  clears automatically on reboot.
- **Explicit cleanup:** `pmux drops clean` clears all drop files immediately.

### 4. Path insertion (typed text vs clipboard vs image)

- **Default ("Upload and paste path"):**
  - Validate the returned remote path contains no control characters or NULs.
  - POSIX-shell-quote any path containing characters outside
    `[A-Za-z0-9/_.-]`, join multiple uploaded paths with a single space, and
    **never append `\r` or `\n`** (never auto-execute).
  - Deliver through `PasteJobs::text` (`paste_job.rs:392`) wrapped in DECSET
    2004 bracketed paste (`paste_payload`, `main.rs:19028`) when
    `host.emulator.bracketed_paste()` is true. This guarantees paste ordering
    with keystrokes and never blocks the main thread.
- **Clipboard ("Upload and copy path"):** writes the remote path(s) to the
  local clipboard via `arboard` and shows a toast; nothing is written to the
  PTY (safe when an editor or full-screen TUI has focus).
- **Images:** dropped image files upload through the exact same
  `pmux drop-receive` flow and insert their remote path as plain text (the host
  cannot reliably inspect the remote foreground process across `ssh` without an
  extra RPC). Reusing `pmux drop-receive` for remote clipboard-image paste
  (`image_paste.rs`) is a natural small follow-up once Slice B lands.

## Windows safety summary

- **Port-forward:**
  - Uses Windows OpenSSH (`ssh.exe`) via `hidden_command("ssh")` (no console
    flash, no Unix-only `ControlMaster` dependency).
  - Attaches spawned `ssh -N` processes to a Windows Job Object with
    `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so a host crash cannot leave orphaned
    localhost listeners behind.
  - Reuses `hyperlink::spawn_open`'s `ShellExecuteW` allowlist
    (`hyperlink.rs:376-407`).
- **File drop:**
  - `winit 0.30.13` enables OLE drag-and-drop on Windows by default
    (`with_drag_and_drop(true)`). The build must verify on a clean Windows VM
    that `winit`'s OLE initialization coexists cleanly with `hyperlink.rs:389`'s
    `CoInitializeEx(COINIT_APARTMENTTHREADED)` call.
  - Rejects UNC paths (`\\server\share`), device paths (`\\.\`), and
    reparse points / `.lnk` shortcuts before opening or reading any file.
  - Never joins remote paths with local `PathBuf` (remote `pmux drop-receive`
    always returns its native path string).
  - If the remote side itself runs Windows, `pmux drop-receive` creates an
    owner-only ACL directory via `private_fs.rs` (no POSIX `0700`/`0600`
    assumption).

## Interaction with future identity-based remotes

A future identity-based remote (such as Tailscale identity login without SSH
keys) replaces OpenSSH authentication and transport while keeping the user
facing workflow identical:

- **Shared across both transports:** `RemotePaneBinding`, localhost URL
  detection and rewriting, click confirmation modal, active forwards table,
  drop validation and consent modal, `PasteJobs` insertion, and remote
  `pmux drop-receive` storage and cleanup.
- **Transport-specific (behind `RemoteTransport`):**
  - *SSH today:* `ssh -N -T -L` for forwards; `ssh -T -- <alias> pmux drop-receive`
    for uploads.
  - *Future identity remote:* multiplexed stream over the authenticated tailnet
    connection to the remote `pmuxd` (or helper) for forwards and uploads,
    preserving loopback-only local binding and explicit confirmation.

No SSH assumptions leak into the UI, modal, or remote drop-storage layers.

## Risks and mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Spoofed OSC 8 link tricks user into forwarding a sensitive remote loopback port (admin API, DB, local agent socket) | Unauthorized local browser access to remote loopback service | Mandatory confirmation modal on every new forward showing parsed destination, remote/local ports, full URL, and an explicit warning when OSC 8 label != target. |
| `GatewayPorts yes` in user's `~/.ssh/config` exposes forwarded port on LAN | Network exposure of remote loopback service | Always pass `-o GatewayPorts=no` and bind `127.0.0.1` explicitly. |
| Accidental drag-drop uploads sensitive local file (e.g., `.env`, key file) | Unintended data transfer to remote host | Mandatory consent modal listing filenames, sizes, and highlighting dotfiles; reject symlinks, directories, special files, and Windows UNC/reparse points. |
| Remote shell injection or `/tmp` symlink attack via crafted filename | Remote code execution or file clobbering | Zero shell interpolation (`pmux drop-receive` reads JSON header on stdin), sanitized basename with random hex prefix, `0700` private dir, `O_CREAT \| O_EXCL \| O_NOFOLLOW` `0600` file. |
| Forgotten background tunnels or orphaned `ssh -N` processes | Stale listening ports | Auto-close forwards when last tab to destination closes; explicit Stop/Stop all in forwards table; kill+reap on host exit; Windows Job Object `KILL_ON_JOB_CLOSE`. |
| Unbounded remote drop directory fills disk | Disk exhaustion on remote host | 64 MiB/file + 128 MiB/batch cap enforced on both host and remote; 24 h TTL + 512 MiB cap pruned automatically; session-destroy cleanup. |
| Wrong split pane receives dropped path | Path pasted into wrong terminal | Consent modal explicitly names target session/pane and offers "Copy path" alternative; never appends Enter. |
| `winit` OLE drag-and-drop conflicts with `ShellExecuteW` COM init on Windows | Drop or link-open fails on Windows | Explicitly test both drag-drop and link-open in the same host process on a clean Windows VM. |

## Build plan and effort estimate

Calibrated against recent `prismattyc-host` and `prismattyc-mux` PRs (such as
remote SSH catalog/attach, `restart_modal`, and `paste_job`, typically 4–6
agent-hours and 1–1.5 wall-clock days per PR including review, real-SSH tests,
and Windows VM proofs). This matches the PI-close median (`18h / 5d`) with
explicit budget for clean-VM proofs.

### Build 1 — Remote localhost port-forward (2 trunk PRs)

| PR | Scope | Agent-hours | Wall-clock days |
| --- | --- | ---: | ---: |
| **PR 1** | `RemotePaneBinding` on `HostState`, `link_click::Gesture::take_due` returning `(PaneId, String)`, localhost URL detection/rewriting, `RemoteTransport` trait + `SshForwardManager` (`ssh -N -L 127.0.0.1:...`, `GatewayPorts=no`, `ExitOnForwardFailure=yes`, Windows Job Object), flag-gated off by default, unit + real-SSH loopback tests | 5–6 h | 1–1.5 d |
| **PR 2** | Forward confirmation modal (with OSC 8 mismatch warning), palette `Forward Port…` + active `Port Forwards` table overlay, last-tab & shutdown teardown, default-on flip, macOS/Linux/Windows proof | 5–6 h | 1.5 d |
| **Subtotal** | **Slice A (port-forward)** | **10–12 h** | **2.5–3 d** |

### Build 2 — Remote file drop (2 trunk PRs)

| PR | Scope | Agent-hours | Wall-clock days |
| --- | --- | ---: | ---: |
| **PR 1** | `pmux drop-receive` + `pmux drops clean` in `prismattyc-mux` (`0700` private dir, `O_EXCL` `0600` file, JSON stdin header + raw stream, size/TTL/session cleanup), host `SshDropUploader` worker, real-SSH upload + traversal/symlink/oversize tests | 4–5 h | 1–1.5 d |
| **PR 2** | `winit` `HoveredFile`/`DroppedFile`/`HoveredFileCancelled` wiring in `prismattyc-host`, UNC/reparse-point/size/special-file guards, drop consent modal (paste vs copy vs cancel), `PasteJobs` bracketed-paste insertion, macOS/Linux/Windows VM proof | 4–5 h | 1.5 d |
| **Subtotal** | **Slice B (file drop)** | **8–10 h** | **2.5–3 d** |

### Recommended build tickets

1. **Build: remote localhost port-forward with confirmation and forwards table**
   (**10–12 agent-hours / 2.5–3 wall-clock days**, Slice A PRs 1–2).
2. **Build: remote file drop with consent and private temp-file cleanup**
   (**8–10 agent-hours / 2.5–3 wall-clock days**, Slice B PRs 1–2; reuses
   `RemotePaneBinding` and `RemoteTransport` from Build 1).

## Proof plan for the builds

- **Port-forward (Slice A):**
  - Real-SSH loopback test (`scripts/remote-ssh-tests.sh` harness): start an
    HTTP server on a remote loopback port, trigger forward, fetch through the
    forwarded local `127.0.0.1` port, verify `GatewayPorts=no` and clean
    teardown when the forward is stopped or the last remote tab closes.
  - Unit tests for localhost URL classification (`localhost`, `127.x`, `0.0.0.0`,
    `[::1]`, `[::]`, non-localhost ignored, OSC 8 mismatch flagged, port
    collision fallback to ephemeral local port, cap enforcement).
  - Windows VM proof: verify `ssh.exe -N` runs without console flash, opens the
    local browser via `ShellExecuteW`, and terminates cleanly via Job Object on
    host exit.
- **File drop (Slice B):**
  - Real-SSH round-trip test: upload a binary file and a filename with spaces
    and shell metacharacters through `pmux drop-receive`; verify exact SHA-256,
    `0700` dir / `0600` file modes, sanitized path, TTL pruning, and session
    cleanup.
  - Security rejection tests: reject oversize files, symlinks, directories,
    traversal names (`../x`), trailing bytes, and Windows UNC / `.lnk` /
    reparse-point paths.
  - Host UI tests: consent modal cancels cleanly without spawning `ssh`,
    "Upload and paste path" delivers via `PasteJobs` with bracketed paste and
    no newline, and "Upload and copy path" writes clipboard only.
  - Windows VM proof: verify `winit` `DroppedFile` + `ShellExecuteW` COM
    coexistence and clean upload to a remote host.

## Out of scope

- Forwarding remote non-loopback / LAN targets (reverse proxies or SOCKS
  tunnels).
- Remote-to-local reverse port forwarding (`ssh -R`).
- Silent/automatic forwarding without user confirmation.
- Directory or recursive folder upload in v1 (regular files only).
- Bidirectional sync or editing local files in place over SSHFS/SFTP.

## Open questions before build

1. **Per-destination "always allow" vs always confirm:** this note recommends
   confirming every new `(destination, remote_port)` (with zero re-prompt on
   re-clicks of an already-active forward). Confirm whether no "trust all ports
   on host" bypass is wanted.
2. **Last-tab teardown vs pinned forwards:** confirm whether auto-stopping
   forwards when the last remote tab to that destination closes is the right
   default, or whether users should have an opt-in "keep while app is open"
   toggle per forward.
3. **Local file drop:** should dropping a file onto a **local** pane paste its
   local path (without a consent modal), or stay out of scope of Slice B?
