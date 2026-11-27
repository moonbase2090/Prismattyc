# Remote attach transport and trust for a companion client

Status: **decision record from a time-boxed spike, not shipped behavior.** No
listener, command or file described here exists yet. Nothing in this change
opens a port or alters how the pmux server accepts connections.

Base: `origin/main` `3bba8e8f`. Line references are to that commit.

Updated for the owner's decisions of November 27, 2026 on the four open
questions: push notifications are in v1, pairing defaults to full input, there
is no public-port fallback, and the companion-only listener is optional and
off by default. The last section lists what each decision changed.

## Summary

A phone or tablet companion needs to watch sessions, type into a pane, and see
which seats need the user. Today the pmux server accepts connections only on a
local socket, and only from the same OS user. This note compares three ways to
reach it from another device and recommends one.

**Decision:** the companion reaches the host over **SSH** carried by an
**overlay VPN** the host already uses, and over nothing else. Each device gets
its own key,
installed with a **restricted `authorized_keys` entry** whose forced command is
a small, fixed **companion gate** (`pmux companion gate`, proposed). The gate
runs on the host as the user, talks to the local socket like any other `pmux`
command, and accepts only a short list of verbs. The companion path never puts
the raw socket on the network, and no caller reaches the gate without a device
key that the user added by hand.

The restricted entry limits only the companion's own keys. Other ways into the
same account, such as passwords or ordinary keys, are not limited by it. The
note states which host settings it assumes, what pairing checks, and an
optional companion-only SSH listener, off by default, that closes those other
paths on the network the companion uses.

Push notifications for the needs-you inbox are sent by the host itself, as a
content-free wake-up through the mobile platform's push service. All terminal
and inbox data still travels only over SSH.

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
- The needs-you inbox sends push notifications in v1.
- Attaching to a Space defaults to full input.
- There is no public-port fallback. When the overlay VPN is unavailable, the
  companion does not connect.
- The companion-only SSH listener is optional and off by default.

Added by this spike:

- The raw local socket is never reachable from the network, directly or
  through SSH stream forwarding.
- Every connection names one enrolled device. Revoking a device takes effect
  on the next connection and ends its live connections.
- A device can be enrolled as view-only, by choice at pairing.
- A push carries no terminal text, seat name or message.
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
NAT without a public port or port forwarding. The owner has ruled out a public
port, so A is not offered on its own.

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
needs something SSH cannot carry, such as sharing a session with another person
or devices with no SSH client. Push wake-ups do not need it: the host sends
them outbound (see Push notifications), with no listener.

### Comparison

| | A. SSH only | B. SSH over overlay | C. Own protocol |
| --- | --- | --- | --- |
| New listener on the host | No (uses the system SSH server) | No | Yes |
| Reach from a mobile network | Needs a public SSH port | Yes, through NAT | Needs a public port or the overlay |
| Push wake-ups | Outbound from the host | Outbound from the host | Outbound, or over its own channel |
| Per-device identity | Yes, with per-device keys | Yes, two layers | Yes |
| Narrow permissions | Yes, with a forced command | Yes, with a forced command | Yes, per request |
| Within the owner's transport policy | No, no public port | Yes | No, not for v1 |
| New code to trust | The gate | The gate | Listener, protocol, crypto, registry |

## Decision

Use **B only**: SSH to the host's system SSH server over the overlay, with one
restricted key per device and a fixed gate as the forced command.

There is no fallback. When the overlay is down on the phone or the host, the
companion shows that the overlay is unreachable and does not try another
address. It does not use a LAN address either: from the phone, a LAN address
cannot be told apart from a forwarded public port, so only the overlay address
from the pairing code is dialed.

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
- `--scope` is `drive` (view and type) by default, or `watch` (view only) when
  the user picks it at pairing.
- Pairing always adds `from="<overlay range>"`, so the key is refused from any
  other network.
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
| `attach SESSION_ID SPACE_ID` | `drive` | `pmux attach --session-id … --space-id …`, with full input. |
| `attach SESSION_ID SPACE_ID` | `watch` | The same attach with `--read-only`. |
| `push-register TOKEN` | both | Stores this device's push token on the host, replacing any earlier one. |
| `push-unregister` | both | Removes this device's push token. |

- Arguments are parsed as identifiers and checked against the catalog. Names
  are never passed through as command text.
- An unknown verb, an extra argument or a parse failure exits non-zero with a
  one-line error. The gate never falls back to a shell.
- The gate passes the same-user check like any local `pmux` command, so the
  pmux server needs no change to accept it.
- The gate records its pid in a per-device runtime file so revocation can end
  live connections.
- A push token is checked as an opaque string of bounded length and stored in
  a per-device file with mode `0600`. It is never passed to a shell.
- The gate does not offer shutdown, plug-in management, `send-keys`, mail
  writes, Space edits or `pmux new`. Adding a verb later is a reviewed change
  to the gate, not a client update.

The inbox can poll the existing `Events` request with a short interval until
the streaming event endpoint lands. It then switches to that endpoint without
changing the gate verb.

### Pairing, without a listener or relay

1. On the host, `pmux companion pair` shows a pairing code (QR and text). It
   carries the gate protocol version, SSH user, overlay address, port and
   **all host key fingerprints**. It contains no secret. Pairing refuses to
   start when the host has no overlay address.
2. The phone scans it, creates a non-exportable P-256 key that needs user
   presence to sign, and shows the public key as text and as a share-sheet item
   with a short fingerprint.
3. The user moves the public key to the host by any channel they already use
   (share sheet, clipboard sync, a file). `pmux companion add` reads it, shows
   the same short fingerprint for the user to compare, asks for the scope with
   `drive` preselected, and writes the restricted line atomically with mode
   `0600`.
4. The phone runs `hello` against the pinned host keys, then `push-register`
   with its push token. On success it stores the host as paired.

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

`pmux companion revoke DEVICE` does four things:

- removes that device's line;
- deletes the device's push token, so the host stops sending it wake-ups;
- signals the device's live gate processes, which ends its open attaches;
- prints what it removed.

`pmux companion list` shows each device, its scope, whether it has a push token
and when it last connected. On the overlay, removing the phone's node is a
second, independent cut-off.

### Push notifications

The owner wants the inbox to notify in v1, including while the app is not
running. A suspended mobile app can only be woken by the platform's push
service, so something must send to that service. The design keeps that sender
on the host and keeps the data on SSH:

1. At pairing, and whenever the platform rotates it, the phone sends its push
   token through the gate's `push-register` verb.
2. On the host, a small sender runs beside the pmux server as a local client,
   like any `pmux` command, and watches the same signals the inbox uses. When
   a seat enters the inbox for an urgent reason (every rank except a finished
   turn, as set out in [`phone-seat-attention.md`](phone-seat-attention.md)),
   the sender sends one wake-up to each registered device of that host.
3. The wake-up is **content-free**: the host label from pairing and a
   generic line such as "A seat needs you". It carries no seat name, message,
   pane text or session id. The platform push service sees only that this host
   sent a wake-up.
4. Tapping it opens the companion, which unlocks the device key with user
   presence, connects over SSH and loads the inbox. The details are fetched
   then, never from the push.

Why this shape:

- The send is an outbound HTTPS request from the host. It opens no listener
  and adds no relay in the data path.
- The device key needs user presence to sign, so the app cannot reconnect
  silently in the background to fetch details. A content-free push avoids
  needing that.
- Lock-screen exposure is limited to the host label.

What it needs, and the trade-off for the owner to know about:

- The platform push service accepts sends only with the app publisher's push
  credential. For the private test build, the paired computers are the owner's
  own, so the credential can live in each computer's OS credential store and
  is read only by the sending code. A leaked credential lets someone send
  notifications to every install of the app. It does not reveal terminal
  data.
- Before a public release, sending from users' computers with the publisher's
  credential is not acceptable. Public distribution needs a small sender
  service that holds the credential and accepts only content-free wake-ups.
  That is a new owner decision, and it is not part of v1.
- Rate limit: at most one wake-up per device per minute, and none while the
  companion holds an open inbox connection to that host.
- Notifications are on by default for every paired device. The companion
  offers a per-host mute, which calls `push-unregister`.

### Other ways into the same account

The gate constrains a connection only when the SSH server authenticated it
with a companion key. The same account can usually also be reached:

- with a password or keyboard-interactive login, if the SSH server allows them;
- with any other key in `authorized_keys` that is not limited by both a forced
  command and forwarding turned off. `restrict` alone still allows a shell
  without a terminal, and `command=` alone still allows forwarding;
- through other credentials the server accepts (certificates, other key files).

What each path reaches differs:

- A password, a key with no `command=` (including a `restrict`-only key), or an
  unconstrained certificate gives a shell, with or without a terminal. From it
  the caller can run `pmux` directly.
- A key with a `command=` but forwarding still on gives no shell. It can still
  forward a local port to the pmux socket and send any control request through
  it.
- Either way the caller reaches the whole control plane, not the gate's verbs.

The gate cannot stop this, and Prismattyc does not edit the host's SSH server
configuration. The design therefore keeps three layers apart.

**What the companion itself does.** The client offers only its own device key.
It never uses an SSH agent, a password or an interactive prompt. A companion
credential therefore always lands in the gate.

**What the design assumes about the host.** On every address and port in the
pairing code:

1. The SSH server accepts public keys only. Password and keyboard-interactive
   logins are off.
2. Every other key the account accepts there belongs to someone the user
   already trusts with a full shell on that account.

When either is false, the companion adds no new exposure, but the account is
only as safe as its weakest SSH credential. Neither the gate nor the overlay
changes that.

**What pairing checks.** `pmux companion pair` and `pmux companion list` report
what they can see, without changing anything:

- They probe the address and port in the pairing code for the authentication
  methods the server offers (an authentication attempt with method `none`).
  A warning is shown when `password` or `keyboard-interactive` is offered.
- They list every entry in the account's `authorized_keys` files that pairing
  did not write, in one of three classes. A key falls in the first class that
  matches:
  - **Full access:** no `command=`. This includes keys with only `restrict`,
    which can still run any command without a terminal.
  - **Can forward:** a `command=`, but forwarding is still on: neither
    `restrict` nor `no-port-forwarding` is present, or a later
    `port-forwarding` option turns it back on. Such a key can forward the pmux
    socket whatever its command is.
  - **Forced command:** a `command=` with forwarding off. The report shows the
    command, because it cannot tell whether that command is itself a shell.
- The report covers the default `authorized_keys` files. A server that reads
  keys from elsewhere is outside what it can see, and the report says so.
- Neither check reads the server's configuration or needs elevated rights.

**Optional hardening: a companion-only listener, off by default.** Pairing
works without it, and Prismattyc never turns it on. A user with admin rights
can give the companion its own SSH listener, for example on the overlay address
only. The settings below are applied by the user, never by Prismattyc:

```
# SSH server configuration, applied by an administrator
Match LocalPort 2222
    AuthenticationMethods publickey
    AuthorizedKeysFile .ssh/prismattyc_companion_keys
    DisableForwarding yes
    ExposeAuthInfo yes
    ForceCommand pmux companion gate --from-auth-info
```

- On this port the server reads only the companion key file, which holds only
  entries pairing wrote.
- Passwords, ordinary keys and every kind of forwarding are refused.
- The gate always runs. `ForceCommand` replaces any per-key `command=`, so the
  gate identifies the device from the key the server reports through
  `ExposeAuthInfo` and looks it up in the companion key file. An unknown key
  exits.

When the ordinary SSH port is not reachable from the overlay, the only SSH path
over the overlay goes through the gate. Setup docs present this as optional
hardening for users who can apply it. Without it, which is the default, the
assumptions above apply.

## Threat note

Assets:

- Terminal contents, which can include secrets printed by programs.
- The ability to type into panes, which equals running commands as the user.
- The pmux control plane.
- The host account's SSH access.

| Threat | Effect | Mitigation | Residual risk |
| --- | --- | --- | --- |
| Anonymous caller on the network | Probes or brute-forces the host | No new listener and no public port: the companion dials only the overlay address. Pairing warns when the server offers password or keyboard-interactive logins. | Key-only login is a host assumption the project cannot enforce. A public SSH port the user opened for other reasons is outside the companion's control. |
| Interception on public Wi-Fi | Reads or alters the session | SSH encryption with host keys pinned at pairing; no trust on first use. | None beyond SSH itself. |
| Socket forwarding to reach the raw control plane | Full control, including shutdown and plug-in admin | For companion keys, `restrict` turns off socket and port forwarding and the forced command cannot be bypassed. On the companion-only listener, `DisableForwarding` covers every key. | On an ordinary SSH port, other keys or a password can still forward the socket. `pmux companion list` reports full-access and can-forward keys. |
| Other credentials on the same account | A shell, or a forwarded pmux socket, bypassing the gate | Out of the gate's reach. Host assumptions are stated; pairing reports offered methods and keys that are not fully constrained; the companion-only listener closes these paths on its port. | Without that listener, the account is as safe as its weakest SSH credential, with or without the companion. |
| Gate command injection | Shell access through crafted verbs | Fixed grammar, no shell, identifiers checked against the catalog, refused input exits. | Gate bugs. The gate is small and gets its own tests. |
| Lost or stolen phone | Someone types into panes | Key needs user presence (biometric or passcode) per connection. Revoke from the host. Watch-only scope for devices that do not need input. | `drive` is the default, so a paired device that is unlocked and in hand can type commands. This is inherent to "full terminal input" and should be stated plainly in the app and at pairing. |
| Malicious app or backup on the phone | Copies the key | Key is non-exportable and never in backups. | A compromised OS can still ask the key store to sign while unlocked. |
| Host key swapped by an attacker | Phishing host collects input | Pinned keys, hard stop, no override. | A user who re-pairs without checking the desktop. The pairing code comes from the desktop screen, not the network. |
| Revoked device keeps a live session | Continued access after revoke | Revoke ends live gate processes. | A short window between key removal and the signal. |
| Overlay account compromise | Attacker joins the overlay | Still needs an SSH credential the server accepts on the overlay. With the companion-only listener as the only SSH path there, that means an enrolled device key. `from=` alone is not trusted. | Without that listener, any credential the account accepts (another key, or a password if allowed) works over the overlay. |
| Secrets in the inbox | Message text exposed on the lock screen | Pushes are content-free. Inbox items carry attention messages, which are capped at 512 bytes, and are shown only inside the unlocked app. | None from the push. In-app display is app behavior. |
| Push credential leaked from a host | Someone sends notifications to every install | Stored in the OS credential store, read only by the sender. Wake-ups carry no data and every detail is fetched over SSH. | Spam or misleading wake-ups until the credential is rotated. A public release needs a sender service instead. |
| Push token leaked | Someone who also has the credential targets one device | Tokens stay on the host in `0600` files, deleted on revoke. | The same as a leaked credential, limited to one device. |

Out of scope:

- A push sender service for public distribution (see Push notifications).
- Sharing a session with another person.

## Proof plan

Run later on an **isolated host account** (a test-only OS user, a disposable
VM, or the container used by `scripts/remote-ssh-tests.sh`). Never touch the
live development server or its pmux server.

Setup:

- A test user with its own home and an isolated `PMUX_SOCKET`.
- An SSH server bound to loopback (or the VM's private interface) with a test
  host key.
- Two test device keys, one installed with the default scope (`drive`) and
  one with `--scope watch`.
- A push sender pointed at a local stub that records requests, never the
  platform service.
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
    `ss -ltn`) before and after a full session. The test SSH server's own
    ports are the only TCP listeners.

Checks for the other ways in (the test SSH server also has a password set for
the test user and one ordinary, unrestricted key):

11. With password login on, the pairing probe reports `password` and shows a
    warning. With it off, no warning. `pmux companion list` classes the
    ordinary key and a `restrict`-only key as full access, a `command=`-only
    key as can forward, and a `restrict,command=` key as forced command.
    Companion entries do not appear. A non-PTY `ssh KEY id` with the
    `restrict`-only key, and `ssh -L` to the socket with the `command=`-only
    key, both succeed, which confirms the classes.
12. On the ordinary port, the unrestricted key still gets a shell and can
    forward the socket. This records the limit stated above; it is expected to
    pass before and after the gate.
13. With the companion-only listener not configured (the default), pairing
    completes and checks 1–10 pass. On the companion-only port configured as
    above, the unrestricted key and the password are both refused, `ssh -L`
    to the socket is refused for the drive key, and a connection with no
    command still lands in `hello`. A key removed from the companion key file
    is refused even with `ForceCommand` present.

Checks for push and reachability:

14. `push-register` stores the token in a `0600` file. A token longer than the
    limit or containing control characters is refused. `revoke` deletes it.
15. A message raised by `pmux attention` makes the stub record exactly one
    request per registered device. Its body contains the host label and the
    generic line, and no seat name, message text or session id. A second
    message within a minute records nothing new.
16. `pmux companion pair` on a host with no overlay address refuses with a
    clear error. The written key line always carries `from=`.

Phone-side checks belong to the prototype spike: key creation with user
presence, the pairing transfer, VPN co-existence, and background behavior.

## Follow-up work

Sizes are active seat work, then wall-clock including review, CI and isolated
proof. They are based on recent pmux CLI pull requests of similar size.

| Slice | Scope | Agent-hours | Wall-clock |
| --- | --- | --- | --- |
| Gate | `pmux companion gate`: grammar, scopes, catalog and attach dispatch, polled inbox, pid file, tests and proof checks 1–7 and 10 | 6–8 | 1.5–2 days |
| Pairing and revocation | `pmux companion pair`, `add`, `list` and `revoke`; atomic `authorized_keys` edits; method probe and full-access key report; `--from-auth-info`; checks 8–9 and 11–13 | 8–11 | 2–2.5 days |
| Push sender | `push-register` and `push-unregister`; token files; content-free sender with rate limit, reading the credential from the OS store; checks 14–15 | 6–9 | 1.5–2 days |
| Inbox on the event stream | Switch `inbox` to the streaming endpoint once it lands | 1–2 | 0.5 day |

The catalog and prototype spike should start from the gate's verb list and
the pairing flow above.

## Owner decisions

The owner answered this note's open questions on November 27, 2026:

| Question | Decision | Changed in this note |
| --- | --- | --- |
| Push notifications for the inbox in v1? | Yes. | New Push notifications section, gate verbs, revocation, threat rows, checks 14–15, Push sender slice. |
| Default scope at pairing? | Full input (`drive`). `watch` stays available as a choice. | Per-device key, pairing step 3, gate table order, lost-phone threat row. |
| Public SSH port when the overlay is missing? | No. SSH over the overlay only. | Decision, option A, comparison, pairing code, anonymous-caller threat row, check 16. |
| Companion-only listener expected or optional? | Optional, off by default. | Summary, Optional hardening, check 13. |

The remaining owner decision is how pushes are sent for a public release,
covered under Push notifications.
