------------------------------ MODULE MailClaim ------------------------------
(***************************************************************************)
(* Mail delivery, claim, commit, release, and restart.                    *)
(*                                                                         *)
(* One step here is one store transaction, or the daemon restart that     *)
(* reopens the same database. The control plane looks up the connection's *)
(* agent and then calls the store. It does not record which connection    *)
(* claimed a letter.                                                       *)
(*                                                                         *)
(*   Store::send, Store::broadcast, Store::claim, Store::commit,          *)
(*   Store::release, set_state_or_delete                                   *)
(*     crates/prismattyc-mux/src/mailbox/store.rs                          *)
(*   mail_hello, mail_agent, mail_send, mail_claim, mail_commit,           *)
(*   mail_release, mail_broadcast                                          *)
(*     crates/prismattyc-mux/src/control.rs                                *)
(*   open_and_held_letters_survive_pmuxd_restart                           *)
(*     crates/prismattyc-mux/tests/mail_restart.rs                         *)
(*   daemon open of the same mail database                                 *)
(*     crates/prismattyc-mux/src/bin/pmuxd.rs                              *)
(*                                                                         *)
(* Defect is "none" for that code. Any other value is a broken variant of *)
(* one step, so a check can be seen to fail. A variant is not the code.   *)
(***************************************************************************)

EXTENDS Integers, FiniteSets

CONSTANTS Agents, Clients, LetterIds, none, Defect

VARIABLES
  phase,        \* letter -> unused | open | held | committed | lost
  recipient,    \* letter -> agent; meaningful once created
  created,      \* letter -> BOOLEAN, stays TRUE after send or broadcast
  seated,       \* client -> agent | none; the in-memory MailHello binding
  claimer,      \* letter -> client | none; ghost, the store does not keep it
  presented,    \* letters a claim has already returned
  redelivered,  \* a later claim returned a letter that was already presented
  actor         \* the step that produced this state

vars == <<phase, recipient, created, seated, claimer, presented, redelivered, actor>>

Defects == {
  "none",
  "restart_drops_held",
  "commit_ignores_recipient",
  "commit_ignores_held",
  "claim_leaves_open",
  "claim_includes_committed",
  "send_duplicates",
  "broadcast_drops_rest",
  "release_deletes"
}

ASSUME Defect \in Defects

Actor(kind, client, id) == [kind |-> kind, client |-> client, id |-> id]

Dummy == CHOOSE id \in LetterIds : TRUE

Some(S) == CHOOSE x \in S : TRUE

Other(S) == CHOOSE x \in (S \ {Some(S)}) : TRUE

Unused == {id \in LetterIds : phase[id] = "unused"}

(***************************************************************************)
(* Actions. A no-op store call (unknown id, or a letter this agent does   *)
(* not hold) is omitted: the code changes zero rows.                      *)
(***************************************************************************)

Hello(c, a) ==
  /\ seated' = [seated EXCEPT ![c] = a]
  /\ actor' = Actor("hello", c, Dummy)
  /\ UNCHANGED <<phase, recipient, created, claimer, presented, redelivered>>

\* Store::send inserts one open row. send_duplicates inserts a second row
\* for the same recipient when another unused id exists.
Send(c, a) ==
  /\ seated[c] # none
  /\ Unused # {}
  /\ LET id1 == Some(Unused)
         duplicate == Defect = "send_duplicates" /\ (Unused \ {id1}) # {}
         id2 == IF duplicate THEN Some(Unused \ {id1}) ELSE id1
         fresh == {id1} \cup (IF duplicate THEN {id2} ELSE {})
     IN /\ phase' = [id \in LetterIds |-> IF id \in fresh THEN "open" ELSE phase[id]]
        /\ recipient' = [id \in LetterIds |-> IF id \in fresh THEN a ELSE recipient[id]]
        /\ created' = [id \in LetterIds |-> IF id \in fresh THEN TRUE ELSE created[id]]
        /\ actor' = Actor("send", c, id1)
        /\ UNCHANGED <<seated, claimer, presented, redelivered>>

\* mail_broadcast walks other live session agents and inserts one open row
\* each, in one transaction. A seated client stands for that agent's session.
\* Two clients on one agent are one recipient. broadcast_drops_rest inserts
\* only one of those recipients.
OtherAgents(c) ==
  {seated[d] : d \in {d2 \in Clients : seated[d2] # none /\ seated[d2] # seated[c]}}

Broadcast(c) ==
  /\ seated[c] # none
  /\ LET targets == OtherAgents(c)
     IN /\ targets # {}
        /\ Cardinality(Unused) >= Cardinality(targets)
        /\ LET keep == IF Defect = "broadcast_drops_rest" /\ Cardinality(targets) >= 2
                       THEN {Some(targets)}
                       ELSE targets
               id1 == Some(Unused)
               two == Cardinality(keep) >= 2
               id2 == IF two THEN Some(Unused \ {id1}) ELSE id1
               fresh == {id1} \cup (IF two THEN {id2} ELSE {})
           IN /\ phase' = [id \in LetterIds |-> IF id \in fresh THEN "open" ELSE phase[id]]
              /\ recipient' = [id \in LetterIds |->
                   IF id = id1 THEN Some(keep)
                   ELSE IF two /\ id = id2 THEN Other(keep)
                   ELSE recipient[id]]
              /\ created' = [id \in LetterIds |-> IF id \in fresh THEN TRUE ELSE created[id]]
              /\ actor' = Actor("broadcast", c, id1)
              /\ UNCHANGED <<seated, claimer, presented, redelivered>>

Mine(c) ==
  {id \in LetterIds :
    /\ seated[c] # none
    /\ recipient[id] = seated[c]
    /\ \/ phase[id] \in {"open", "held"}
       \/ (Defect = "claim_includes_committed" /\ phase[id] = "committed")}

\* Store::claim marks every one of this agent's rows held and returns all of
\* them, including rows that were already held. claim_leaves_open returns
\* them and leaves open rows open. claim_includes_committed also returns
\* rows that commit had deleted.
Claim(c) ==
  /\ Mine(c) # {}
  /\ phase' = [id \in LetterIds |->
       IF Defect # "claim_leaves_open" /\ id \in Mine(c) THEN "held" ELSE phase[id]]
  /\ claimer' = [id \in LetterIds |->
       IF id \in Mine(c) /\ phase[id] \in {"open", "committed"} THEN c ELSE claimer[id]]
  /\ redelivered' = (redelivered \/ ((Mine(c) \cap presented) # {}))
  /\ presented' = presented \cup Mine(c)
  /\ actor' = Actor("claim", c, Some(Mine(c)))
  /\ UNCHANGED <<recipient, created, seated>>

CommitCandidates(c) ==
  {id \in LetterIds :
    /\ seated[c] # none
    /\ \/ /\ Defect # "commit_ignores_held"
          /\ phase[id] = "held"
       \/ /\ Defect = "commit_ignores_held"
          /\ phase[id] \in {"open", "held"}
    /\ \/ recipient[id] = seated[c]
       \/ Defect = "commit_ignores_recipient"}

\* Store::commit deletes rows this agent holds. One id here is one id in
\* that call. A multi-id call is one transaction of these per-id deletes.
Commit(c) ==
  /\ CommitCandidates(c) # {}
  /\ LET id == Some(CommitCandidates(c))
     IN /\ phase' = [phase EXCEPT ![id] = "committed"]
        /\ actor' = Actor("commit", c, id)
        /\ UNCHANGED <<recipient, created, seated, claimer, presented, redelivered>>

ReleaseCandidates(c) ==
  {id \in LetterIds :
    seated[c] # none /\ phase[id] = "held" /\ recipient[id] = seated[c]}

\* Store::release sets held rows back to open. release_deletes removes the
\* row instead.
Release(c) ==
  /\ ReleaseCandidates(c) # {}
  /\ LET id == Some(ReleaseCandidates(c))
     IN /\ phase' = [phase EXCEPT ![id] =
          IF Defect = "release_deletes" THEN "lost" ELSE "open"]
        /\ claimer' = [claimer EXCEPT ![id] = none]
        /\ actor' = Actor("release", c, id)
        /\ UNCHANGED <<recipient, created, seated, presented, redelivered>>

\* Process exit and a new process opening the same database. Bindings live
\* in memory, so they come back empty. Rows are unchanged.
\* restart_drops_held throws away held rows.
Restart ==
  /\ \/ \E c \in Clients : seated[c] # none
     \/ \E id \in LetterIds : phase[id] = "held"
  /\ seated' = [c \in Clients |-> none]
  /\ phase' = [id \in LetterIds |->
       IF Defect = "restart_drops_held" /\ phase[id] = "held" THEN "lost" ELSE phase[id]]
  /\ actor' = Actor("restart", none, Dummy)
  /\ UNCHANGED <<recipient, created, claimer, presented, redelivered>>

Init ==
  /\ phase = [id \in LetterIds |-> "unused"]
  /\ recipient = [id \in LetterIds |-> Some(Agents)]
  /\ created = [id \in LetterIds |-> FALSE]
  /\ seated = [c \in Clients |-> none]
  /\ claimer = [id \in LetterIds |-> none]
  /\ presented = {}
  /\ redelivered = FALSE
  /\ actor = Actor("init", none, Dummy)

Next ==
  \/ \E c \in Clients, a \in Agents : Hello(c, a)
  \/ \E c \in Clients, a \in Agents : Send(c, a)
  \/ \E c \in Clients : Claim(c)
  \/ \E c \in Clients : Commit(c)
  \/ \E c \in Clients : Release(c)
  \/ \E c \in Clients : Broadcast(c)
  \/ Restart

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* What the store and the restart test keep.                               *)
(***************************************************************************)

\* A stored letter is still open, still held, or was committed.
NoLetterLost ==
  \A id \in LetterIds : created[id] => phase[id] \in {"open", "held", "committed"}

\* Commit removes a letter. A later step does not bring it back.
CommittedStaysGone ==
  [][\A id \in LetterIds : phase[id] = "committed" => phase'[id] = "committed"]_vars

\* A commit that removes a letter removes one the acting connection's agent
\* currently holds.
CommitOnlyByRecipient ==
  [][
    actor'.kind = "commit" =>
      /\ phase[actor'.id] = "held"
      /\ recipient[actor'.id] = seated[actor'.client]
      /\ phase'[actor'.id] = "committed"
  ]_vars

\* One send stores one letter.
SendStoresOne ==
  [][
    actor'.kind = "send" =>
      Cardinality({id \in LetterIds : phase[id] = "unused" /\ phase'[id] # "unused"}) = 1
  ]_vars

\* Restart leaves every row where it was. Held stays held.
RestartKeepsMailbox ==
  [][
    actor'.kind = "restart" =>
      \A id \in LetterIds : phase'[id] = phase[id]
  ]_vars

\* Claim holds every open or held letter of that agent.
ClaimHoldsOpen ==
  [][
    actor'.kind = "claim" =>
      \A id \in LetterIds :
        (recipient[id] = seated[actor'.client] /\ phase[id] \in {"open", "held"})
        => phase'[id] = "held"
  ]_vars

\* Broadcast stores one new open letter for every other seated agent.
BroadcastOneEach ==
  [][
    actor'.kind = "broadcast" =>
      \A a \in OtherAgents(actor'.client) :
        \E id \in LetterIds :
          phase[id] = "unused" /\ phase'[id] = "open" /\ recipient'[id] = a
  ]_vars

\* Release returns that held letter to open and leaves every other letter.
ReleaseReturnsToOpen ==
  [][
    actor'.kind = "release" =>
      /\ phase[actor'.id] = "held"
      /\ recipient[actor'.id] = seated[actor'.client]
      /\ phase'[actor'.id] = "open"
      /\ \A id \in LetterIds \ {actor'.id} : phase'[id] = phase[id]
  ]_vars

(***************************************************************************)
(* Wording that the store does not implement. The code check does not use  *)
(* these. The ticket check does, and it is expected to fail.               *)
(*   RestartReturnsHeldToOpen: a restart would put held letters back.      *)
(*   CommitOnlySameSession: only the connection that claimed may commit.   *)
(*   AtMostOnceClaim: a claim never returns a letter it already returned.  *)
(***************************************************************************)

RestartReturnsHeldToOpen ==
  [][
    actor'.kind = "restart" =>
      \A id \in LetterIds : phase[id] = "held" => phase'[id] = "open"
  ]_vars

CommitOnlySameSession ==
  [][
    actor'.kind = "commit" => claimer[actor'.id] = actor'.client
  ]_vars

AtMostOnceClaim == redelivered = FALSE

=============================================================================
