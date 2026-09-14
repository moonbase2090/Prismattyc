# prismattyc-labs

Browser labs POC (PT-106): in-memory agent mail + splash `art_frame` for
wasm32. No PTYs, sockets, or pmuxd in this crate.

The mailbox is a **teaching mirror** of the real pmux mailbox. Keep it true
to `prismattyc-mux/src/mailbox/store.rs` and the `Mail*` handlers in
`prismattyc-mux/src/control.rs` (PT-117):

- identity is bound to the `Lab`, never a call parameter — the daemon
  derives `from` from the connection, so `mailSend` cannot forge a sender;
- claim, commit, release, and peek act on the bound identity only;
- `mailCommit(ids)` / `mailRelease(ids)` are **id-scoped**: a partial
  commit leaves the rest held, and unknown or foreign ids are ignored;
- `mailPeek()` returns `{ open, held }` counts — `mailClaim()` is the only
  path to letter bodies;
- recipient ids obey the `AgentId` rules (2–64 chars, `[a-z0-9-]`, no
  leading, trailing, or doubled hyphen) and report the daemon's error text.

It mirrors a **subset**. The real daemon also resolves aliases as `to`,
refuses any `Mail*` before `MailHello`, and serves `MailWait`, `MailWho`,
and `MailBroadcast`. None of those exist here; do not read their absence
as the contract.

## JS surface

```js
import init, { Lab } from './pkg/prismattyc_labs.js';
await init();

const lab = new Lab('operator-a');          // throws on a bad id
const { id, depth } = lab.mailSend('operator-b', 'tests green', 'all good');
lab.setIdentity('operator-b');              // seat switch, not cross-agent read
lab.mailPeek();                             // { open: 1, held: 0 }
const letters = lab.mailClaim();            // bodies arrive here only
lab.mailCommit([letters[0].id]);            // id-scoped; returns a count
lab.splashFrame(performance.now());         // plain number, no BigInt
```

## Native

```bash
cargo test -p prismattyc-labs
```

Mailbox and agent-id unit tests run on the host. The `Lab` wasm-bindgen API
is compiled only for `target_arch = "wasm32"`; CI gates it with:

```bash
rustup target add wasm32-unknown-unknown
cargo check -p prismattyc-labs --target wasm32-unknown-unknown --locked
```

## WASM package (website)

From this repository checkout, with `wasm-pack` on PATH:

```bash
wasm-pack build crates/prismattyc-labs --target web --out-dir <site>/labs/pkg
```

`<site>` is an **absolute** path to the `prismattyc-website` checkout;
`--out-dir` resolves relative to the crate directory, not the shell's.
Omit it to write `crates/prismattyc-labs/pkg/` instead. Website UI lives in
`prismattyc-website`, not this repository.
