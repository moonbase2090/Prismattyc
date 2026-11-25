---
name: verify-mail-claim
description: >-
  Check the pmux mailbox claim, commit, release, and restart steps after a
  change to mailbox storage or the mail control handlers. Run the model
  check, match a failing trace to the store, and update the spec when a
  step in the code changed.
---

# Verify mail claim

The check is a TLA+ model of one mailbox. TLC explores orders of the steps. Read a failing trace, and a passing run's state chart, with `/statechart`. After a merge or a refactor that changed this code without that read, use `/maintain-formal-verification-skill`.

## When it applies

Apply this skill when a change touches any of these:

- `Store::send`, `Store::broadcast`, `Store::claim`, `Store::commit`, `Store::release`, or `set_state_or_delete` in `crates/prismattyc-mux/src/mailbox/store.rs`
- `mail_hello`, `mail_agent`, `mail_send`, `mail_claim`, `mail_commit`, `mail_release`, or `mail_broadcast` in `crates/prismattyc-mux/src/control.rs`
- the mailbox open in `crates/prismattyc-mux/src/bin/pmuxd.rs`
- `open_and_held_letters_survive_pmuxd_restart` in `crates/prismattyc-mux/tests/mail_restart.rs`

A change that does not touch those can skip the check.

## Run

From the repository root:

```sh
specs/mail-claim/check.sh
```

The script pins TLC to the `v1.8.0` `tla2tools.jar` (build `2026.11.19.180144`, sha256 `fdffef247f563ef6edbfd44958d2a82db79e7564e07cb673262d896353fa6713`) and needs Java 11 or newer. The run is done when it prints:

```text
mail claim model: code contract holds; known wordings still fail
```

That line means two things. The code contract found no error at the bounds below. The three wordings in Known wordings still fail on those same steps.

## Read a failure

A code-contract failure names one of `NoLetterLost`, `CommittedStaysGone`, `CommitOnlyByRecipient`, `SendStoresOne`, `RestartKeepsMailbox`, `ClaimHoldsOpen`, `BroadcastOneEach`, or `ReleaseReturnsToOpen`.

Replay that trace on the functions above, one model step for one transaction. `Hello` is `mail_hello`. `Send` is `Store::send`. `Broadcast` is `mail_broadcast`. `Claim` is `Store::claim`. `Commit` is `Store::commit`. `Release` is `Store::release`. `Restart` is a new process opening the same database: `client_agents` is empty and letter rows are unchanged.

Call it a bug when every step can happen in those functions and the named property fails. State the harm in one sentence and keep the property. Call it a modeling error when a step cannot happen. Fix the action so it matches the function, then run the check again.

A variant line says the broken step failed the property that guards it. A variant that passes means that guard can no longer fail. Repair the variant in `specs/mail-claim/check.sh` or the action it breaks, then run the check again.

## Update the model

The sync commit is `bdf2866c964c90e5c82ba2d2770656b7ef764995`. Diff the covered files from that commit to the commit you are checking. When they differ, edit `specs/mail-claim/MailClaim.tla` so each action matches the function again. One store transaction stays one action. Move the sync commit in this skill only after `specs/mail-claim/check.sh` has been run on that updated model.

`Defect` stays `"none"` for the code. The other defect values exist so the script can show each code-contract property failing. Leave them in place when you change an action.

## Code contract

Each property is one sentence. The harm is the sentence under it.

- `NoLetterLost`: a letter that was stored is open, held, or committed. Harm: a restart or a release throws the letter away.
- `CommittedStaysGone`: a committed letter stays gone. Harm: a claim hands the letter out again after it was acknowledged.
- `CommitOnlyByRecipient`: a commit removes a letter only when the acting connection's agent holds it. Harm: another agent acknowledges the letter, or an open letter is deleted.
- `SendStoresOne`: one send stores one letter. Harm: one send delivers two copies.
- `RestartKeepsMailbox`: a restart leaves every letter in the state it had. Harm: held mail is dropped or put back on restart.
- `ClaimHoldsOpen`: a claim marks that agent's open letters held. Harm: the letter stays open and can be claimed again as new.
- `BroadcastOneEach`: a broadcast stores one open letter for each other seated agent, in one step. Harm: one of those agents never gets the letter.
- `ReleaseReturnsToOpen`: a release sets that held letter back to open. Harm: the letter is deleted, or some other letter moves.

## Known wordings

These three are checked against the same steps and fail. The store comments, `open_and_held_letters_survive_pmuxd_restart`, and `held_letters_survive_reopen_and_relist` describe the behavior the traces show. A future run where one of them passes means the code changed. Read the store before treating the wording as the contract.

- `RestartReturnsHeldToOpen`. Wording: a restart puts held letters back to open. Trace: hello, send, claim, restart, and the letter is still held. Harm the wording is about: a held letter is stuck after a daemon restart. The code keeps it held so the same agent can claim and commit after reconnect.
- `CommitOnlySameSession`. Wording: only the connection that claimed a letter can commit it. Trace: two connections hello as the same agent, the first claims, the second commits, and the letter is gone. Harm the wording is about: a different connection acknowledges the letter. The code keys the row by recipient agent. `mail_commit` loads that agent from the connection and deletes a held row for that agent. A different agent's commit deletes nothing.
- `AtMostOnceClaim`. Wording: a claim never returns a letter it already returned. Trace: hello, send, claim, claim again, and the same held letter is returned. Harm the wording is about: the body is shown twice. The code re-lists held letters on purpose.

## Bounds

Three agents (`a`, `b`, `d`), three connections, two letters. Commit and release act on one id, which is one id inside `set_state_or_delete`. A multi-id call is that transaction repeated for each parsed id. Broadcast's recipients are the other agents that currently have a connection. The reachable graph for the code contract is 252779 distinct states and depth 13.

Left out: mailbox rename and aliases, the in-memory lab mailbox (it mirrors claim, commit, and release and has no restart), `MailWait`, the doorbell, depth peek, letter text, and a crash inside a SQLite transaction. The store mutex makes the transactions the interleaving. An unknown id or a letter this agent does not hold changes zero rows, so the model omits that call.

## Tools

TLA+ is the check because the question is whether these steps can happen in a bad order. A verifier aimed at one Rust function would not explore that order. TLC `v1.8.0` is the pinned checker.
