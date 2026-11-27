# Remote attach transport and trust for a companion client

Status: **decision record from a time-boxed spike, not shipped behavior.** No
listener, command or file described here exists yet. Nothing in this change
opens a port or alters how the pmux server accepts connections.

Base: `origin/main` `3bba8e8f`. Line references are to that commit.

## Summary

A phone or tablet companion needs to watch sessions, type into a pane, and see
which seats need the user. Today the pmux server accepts connections only on a
local socket, and only from the same OS user. This note compares three ways to
reach it from another device and recommends one.

**Decision:** the companion reaches the host over **SSH**, preferably carried
by an **overlay VPN** the host already uses. Each device gets its own key,
installed with a **restricted `authorized_keys` entry** whose forced command is
a small, fixed **companion gate** (`pmux companion gate`, proposed). The gate
runs on the host as the user, talks to the local socket like any other `pmux`
command, and accepts only a short list of verbs. The raw socket never leaves
the host, and no caller reaches the gate without a device key that the user
added by hand.

A dedicated network protocol for the pmux server is deferred. The gate's stdio
framing is designed so that the same protocol could later run over another
authenticated carrier without changing the client's verbs.

## What exists today

- **The socket is local and same-user only.** `ControlServer::bind`
  (`crates/prismattyc-mux/src/control.rs:7615`) binds a Unix socket and sets it
  to mode `0600`. Every accepted connection passes `verify_same_user`
  (`control.rs:8591`, `getpeereid` on macOS, `SO_PEERCRED` on Linux). Windows
  relies on a user-only parent directory ACL.
- **There are no per-request permissions.** Once a peer passes the same-user
  check it can send every `ControlRequest`, including shutdown, plug-in
  management and writes to any pane. Forwarding this socket to another device
  would hand that device the whole control plane.
- **SSH is already the remote path.** `docs/ssh.md` documents
  `ssh -t host 'pmux attach work'`. The desktop remote Space catalog
  (`docs/design/remote-spaces-ssh.md`) runs
  `ssh -T -o BatchMode=yes -- ALIAS pmux space catalog` for discovery and
  `ssh -t ... pmux attach --session-id ID --space-id SPACE` for one attach
  (`crates/prismattyc-host/src/remote_catalog.rs:38` and `:56`). It uses the
  system SSH configuration and host-key checking, with key or agent
  authentication only.
- **Remote reads use a vetted argv, not a listener.** The activity monitor
  reads another host with `--host-cmd LABEL JSON_ARGV`, for example
  `["ssh","host","pmux","activity","tail","--from-oldest"]`
  (`docs/activity-monitor-ui.md:116`).
- **Attach already has a view-only mode.** `pmux attach --read-only` never
  takes the input lease (`docs/mux-cli.md:870`). Shared attach hands the
  input lease to the first client that types.
- **Attention signals exist.** `RaiseAttention` stores a message per pane and
  emits `PaneAttention` (`docs/mux-cli.md:404`). The streaming event endpoint
  and `child_exited` events proposed in
  `docs/design/json-events-and-pane-queries.md` are not on main yet.

## Requirements

From the feature's owner decisions:

- Phones and tablets on both mobile platforms ship together in v1.
- Transport is SSH or the overlay VPN only. No relay run by the project, no
  anonymous listener.
- v1 covers watching, full terminal input, and a needs-you inbox of seats.

Added by this spike:

- The raw local socket is never reachable from the network, directly or
  through SSH stream forwarding.
- Every connection names one enrolled device. Revoking a device takes effect
  on the next connection and ends its live connections.
- A device can be enrolled as view-only.
- First pairing does not trust a host key on first use.

## Options

### A. SSH as the only network path

The phone runs an SSH client and connects to the host's system SSH server, the
same way the desktop remote Space catalog does.

| Question | Answer |
| --- | --- |
| Who is identified | The OS account on the host, through the device's public key. With a per-device entry, the forced command also names the device. |
| What a client may do | With an ordinary key, everything the account can do: a login shell, port and socket forwarding, agent forwarding. With a restricted entry (see the decision), only the gate's verbs. |
| Where keys live | The device key is created in the phone's hardware key store and cannot be exported. The host keeps only the public key in `~/.ssh/authorized_keys`. Host keys stay with the SSH server. |
| First pairing | The desktop shows a pairing code carrying the host key fingerprints, so the first connection is checked against known keys rather than trusted on first use. The device's public key reaches the host through a transfer the user starts (details under the decision). |
| Changed host key | The connection stops. The client shows the expected and offered fingerprints and has no "connect anyway" choice. The user re-pairs from the desktop. |
| Untrusted networks | SSH encrypts the session and the pinned host key defeats an interception attempt. To reach a home or office machine from a mobile network, though, the SSH port must be reachable from the internet, which invites scanning and password guessing. The project cannot see or enforce the host's SSH server settings. |

Reachability is the weak point. A phone usually cannot reach a desktop behind
NAT without a public port or port forwarding.

### B. SSH carried by an overlay VPN already on the host

The host and phone both join an overlay VPN (a WireGuard-based mesh). The phone
connects to the host's system SSH server on its overlay address. Everything in
option A still applies, and the overlay adds a network layer.

| Question | Answer |
| --- | --- |
| Who is identified | Two layers. The overlay identifies the device node and its overlay account. SSH identifies the per-device key as in A. |
| What a client may do | The same as A. The overlay's access rules can also limit which devices reach the SSH port at all. |
| Where keys live | Overlay node keys are held by the overlay client on each device. The SSH device key stays in the phone's hardware key store. |
| First pairing | The overlay is joined through its own sign-in, outside Prismattyc. SSH pairing is the same as A, with the overlay address in the pairing code. |
| Changed host key | The same as A. A changed overlay node key is the overlay's business and does not loosen the SSH check. |
| Untrusted networks | The SSH server can listen only on the overlay interface, so nothing is exposed to the internet. Overlay traffic is encrypted end to end and gets through NAT without port forwarding. An entry can also be bound to the overlay's address range with `from=`. |

Some overlays also offer their own SSH server that authenticates the overlay
account instead of an SSH key. This note does **not** recommend it:

- It does not read `authorized_keys`, so forced commands do not apply and every permitted
  device gets a full login shell.
- Who may connect moves into the overlay's admin policy, outside the host
  account.

Platform limits to carry into the prototype:

- Both mobile platforms allow one active system VPN at a time. A user who needs
  a work VPN on the phone loses the overlay while it is on.
- Embedding an overlay client inside the companion app avoids that limit, but
  adds a large dependency and its own sign-in. That is not for v1.

### C. A future authenticated control protocol

The pmux server (or a separate process) would listen on a network port and
speak a versioned protocol with its own device enrollment, mutual
authentication and per-device grants mapped onto `ControlRequest`.

| Question | Answer |
| --- | --- |
| Who is identified | The enrolled device, through a key the server issues or records at pairing. No OS account is involved. |
| What a client may do | Exactly the grants recorded for that device, checked per request. This gives the finest control of the three options. |
| Where keys live | The device key in the phone's hardware store. A server key and a device registry on the host, both of which must be protected. |
| First pairing | A pairing code carrying the server's key and a one-time enrollment secret, scanned from the desktop. |
| Changed host key | The client stops, as with SSH. Key rotation needs its own signed hand-over. |
| Untrusted networks | A new internet-facing listener with new cryptography, replay and rate-limit handling, and its own audit history. On an overlay it could listen on the overlay interface only. |

The owner's transport policy rules out a new listener for now. C also repeats
work SSH already does well. It becomes worth building only if the companion
needs something SSH cannot carry. Examples are push wake-ups, sharing a session
with another person, or devices with no SSH client.

### Comparison

| | A. SSH only | B. SSH over overlay | C. Own protocol |
| --- | --- | --- | --- |
| New listener on the host | No (uses the system SSH server) | No | Yes |
| Reach from a mobile network | Needs a public SSH port | Yes, through NAT | Needs a public port or the overlay |
| Per-device identity | Yes, with per-device keys | Yes, two layers | Yes |
| Narrow permissions | Yes, with a forced command | Yes, with a forced command | Yes, per request |
| Within the owner's transport policy | Yes | Yes | No, not for v1 |
| New code to trust | The gate | The gate | Listener, protocol, crypto, registry |

## Decision

Use **B as the default and A as the fallback**: SSH to the host's system SSH
server, over the overlay when one is present, with one restricted key per
device and a fixed gate as the forced command.

### Per-device restricted key

Pairing writes one line per device (wrapped here; it is one line on disk):

```
restrict,pty,command="pmux companion gate --device dev_7f3a --scope drive"
  ecdsa-sha2-nistp256 AAAA... prismattyc-companion dev_7f3a
```

- `restrict` turns off port, socket, agent and X11 forwarding and the user's
  `~/.ssh/rc`. Turning off socket forwarding matters: without it, an SSH client
  could forward the pmux socket path to the phone and reach the whole control
  plane.
- `pty` is turned back on because attach is a terminal client.
- `command=` makes the SSH server run the gate no matter what the client asks
  for. The client's requested command is only data, in `SSH_ORIGINAL_COMMAND`.
- `--device` is written by the host, not supplied by the client, so the gate
  knows which device is calling.
- `--scope` is `watch` (view only) or `drive` (view and type).
- When the overlay is used, pairing can add `from="<overlay range>"` so the key
  is refused from any other network.
- P-256 is used because it is the key type both mobile hardware key stores
  support. The SSH server accepts it by default.

### The companion gate

The gate is a small `pmux` subcommand. It reads `SSH_ORIGINAL_COMMAND`, splits
it on whitespace (no shell, no quoting, no expansion), and matches it against a
fixed grammar:

| Verb | Scope | Runs |
| --- | --- | --- |
| `hello` | both | Prints the gate protocol version, host label and scope. |
| `catalog` | both | `pmux space catalog` (existing, read-only). |
| `inbox` | both | Streams needs-you items: `PaneAttention` and seat activity states, as NDJSON. |
| `attach SESSION_ID SPACE_ID` | `watch` | `pmux attach --session-id … --space-id … --read-only`. |
| `attach SESSION_ID SPACE_ID` | `drive` | The same attach, without `--read-only`. |

- Arguments are parsed as identifiers and checked against the catalog. Names
  are never passed through as command text.
- An unknown verb, an extra argument or a parse failure exits non-zero with a
  one-line error. The gate never falls back to a shell.
- The gate passes the same-user check like any local `pmux` command, so the
  pmux server needs no change to accept it.
- The gate records its pid in a per-device runtime file so revocation can end
  live connections.
- The gate does not offer shutdown, plug-in management, `send-keys`, mail
  writes, Space edits or `pmux new`. Adding a verb later is a reviewed change
  to the gate, not a client update.

The inbox can poll the existing `Events` request with a short interval until
the streaming event endpoint lands. It then switches to that endpoint without
changing the gate verb.

### Pairing, without a listener or relay

1. On the host, `pmux companion pair` shows a pairing code (QR and text). It
   carries the gate protocol version, SSH user, overlay and LAN addresses, port
   and **all host key fingerprints**. It contains no secret.
2. The phone scans it, creates a non-exportable P-256 key that needs user
   presence to sign, and shows the public key as text and as a share-sheet item
   with a short fingerprint.
3. The user moves the public key to the host by any channel they already use
   (share sheet, clipboard sync, a file). `pmux companion add` reads it, shows
   the same short fingerprint for the user to compare, asks for the scope, and
   writes the restricted line atomically with mode `0600`.
4. The phone runs `hello` against the pinned host keys. On success it stores
   the host as paired.

Step 3 is the clumsiest step. The prototype spike should time it on real
devices. Any shortcut must still keep enrollment on the host and must not admit
an unenrolled caller.

### Host key changes

The client compares the offered key with its pinned set and stops on a
mismatch, with no override. One exception keeps planned rotation working: over
a connection already verified with a pinned key, the client may accept extra
host keys the server announces (the SSH `hostkeys` update extension). It never
accepts a replacement on an unverified connection.

### Revocation

`pmux companion revoke DEVICE` does three things:

- removes that device's line;
- signals the device's live gate processes, which ends its open attaches;
- prints what it removed.

`pmux companion list` shows each device, its scope and when it last connected.
On the overlay, removing the phone's node is a second, independent cut-off.

## Threat note

Assets:

- Terminal contents, which can include secrets printed by programs.
- The ability to type into panes, which equals running commands as the user.
- The pmux control plane.
- The host account's SSH access.

| Threat | Effect | Mitigation | Residual risk |
| --- | --- | --- | --- |
| Anonymous caller on the network | Probes or brute-forces the host | No new listener. SSH key-only; on the overlay, SSH not exposed to the internet. | Option A's public SSH port depends on host SSH settings the project cannot enforce. Docs should say so. |
| Interception on public Wi-Fi | Reads or alters the session | SSH encryption with host keys pinned at pairing; no trust on first use. | None beyond SSH itself. |
| Socket forwarding to reach the raw control plane | Full control, including shutdown and plug-in admin | `restrict` turns off socket and port forwarding; the forced command cannot be bypassed. | A hand-edited entry without `restrict` loses this. `pmux companion list` should flag such entries. |
| Gate command injection | Shell access through crafted verbs | Fixed grammar, no shell, identifiers checked against the catalog, refused input exits. | Gate bugs. The gate is small and gets its own tests. |
| Lost or stolen phone | Someone types into panes | Key needs user presence (biometric or passcode) per connection. Revoke from the host. Watch-only scope for devices that do not need input. | A `drive` device that is unlocked and in hand can type commands. This is inherent to "full terminal input" and should be stated plainly in the app. |
| Malicious app or backup on the phone | Copies the key | Key is non-exportable and never in backups. | A compromised OS can still ask the key store to sign while unlocked. |
| Host key swapped by an attacker | Phishing host collects input | Pinned keys, hard stop, no override. | A user who re-pairs without checking the desktop. The pairing code comes from the desktop screen, not the network. |
| Revoked device keeps a live session | Continued access after revoke | Revoke ends live gate processes. | A short window between key removal and the signal. |
| Overlay account compromise | Attacker joins the overlay | Still needs an enrolled SSH key; `from=` alone is not trusted. | Network reach to the SSH port. |
| Secrets in the inbox | Message text exposed on the lock screen | Inbox items carry attention messages, which are capped at 512 bytes. The app chooses lock-screen redaction. | App behavior, out of scope here. |

Out of scope:

- Push notifications while the app is not running. Waking a suspended mobile
  app needs a push service holding a sender key, which acts as a relay. That is
  an owner decision. Without it, v1's inbox updates only while the app is open
  or briefly in the background.
- Sharing a session with another person.

## Proof plan

Run later on an **isolated host account** (a test-only OS user, a disposable
VM, or the container used by `scripts/remote-ssh-tests.sh`). Never touch the
live development server or its pmux server.

Setup:

- A test user with its own home and an isolated `PMUX_SOCKET`.
- An SSH server bound to loopback (or the VM's private interface) with a test
  host key.
- Two test device keys, one installed with `--scope watch` and one with
  `--scope drive`.
- The client is the system `ssh` with `-o BatchMode=yes` and a test
  known-hosts file, standing in for the phone.

Checks (each must fail before the gate exists and pass after):

1. `ssh -T dev-drive` with no command prints `hello` output, not a shell
   prompt.
2. `ssh dev-drive 'sh -c id'`, `ssh dev-drive 'catalog; id'` and an unknown
   verb each exit non-zero, and `id` never runs. Check the account's process
   accounting or a marker file.
3. `ssh -L 127.0.0.1:PORT:$PMUX_SOCKET dev-drive` and
   `ssh -L 127.0.0.1:PORT:127.0.0.1:22 dev-drive` are refused by the SSH
   server.
4. `ssh -A` agent forwarding is refused (no `SSH_AUTH_SOCK` inside the gate).
5. `catalog` returns the same JSON as a local `pmux space catalog`.
6. `attach` with the watch key shows pane output, and typed bytes never reach
   the pane. Check with a pane running `cat > marker`. With the drive key, the
   bytes arrive.
7. `inbox` shows a message raised by `pmux attention` within one poll interval.
8. A replaced host key makes the client exit before authentication, with the
   expected and offered fingerprints in the error.
9. `pmux companion revoke dev-drive` during a live attach ends that attach and
   makes the next connection fail. The watch device is unaffected.
10. No new listening socket appears: compare `lsof -nP -iTCP -sTCP:LISTEN` (or
    `ss -ltn`) before and after a full session.

Phone-side checks belong to the prototype spike: key creation with user
presence, the pairing transfer, VPN co-existence, and background behavior.

## Follow-up work

Sizes are active seat work, then wall-clock including review, CI and isolated
proof. They are based on recent pmux CLI pull requests of similar size.

| Slice | Scope | Agent-hours | Wall-clock |
| --- | --- | --- | --- |
| Gate | `pmux companion gate`: grammar, scopes, catalog and attach dispatch, polled inbox, pid file, tests and proof checks 1–7 and 10 | 6–8 | 1.5–2 days |
| Pairing and revocation | `pmux companion pair`, `add`, `list` and `revoke`; atomic `authorized_keys` edits; checks 8–9 | 6–9 | 1.5–2 days |
| Inbox on the event stream | Switch `inbox` to the streaming endpoint once it lands | 1–2 | 0.5 day |

The catalog and prototype spike should start from the gate's verb list and
the pairing flow above.

## Open questions for the owner

1. Is push notification for the inbox wanted in v1? It needs a push sender,
   which the current transport policy does not cover.
2. Should `watch` be the default scope at pairing, with `drive` as an explicit
   choice?
3. When no overlay is present, should the client allow option A on a public
   SSH port, or refuse and point to the overlay?
