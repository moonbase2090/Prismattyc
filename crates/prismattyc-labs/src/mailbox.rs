//! In-memory agent mailbox for the browser lab.
//!
//! Mirrors the `prismattyc-mux` mailbox lifecycle (open → held →
//! commit/release) without SQLite or Unix sockets — pure client WASM per
//! PT-106. The contract this file must keep true (see
//! `prismattyc-mux/src/mailbox/store.rs`):
//!
//! - letters are keyed by recipient agent id, never by session;
//! - **open** letters wait to be claimed; **held** letters are claimed
//!   but uncommitted;
//! - `claim` returns every uncommitted letter (open become held,
//!   already-held are re-listed — the reconnect recovery path);
//! - `commit` deletes held letters **by id**; `release` returns held
//!   letters **by id** to open; ids this agent does not currently hold
//!   are ignored, not an error;
//! - `depth` reports both tiers, `(open, held)`.
//!
//! The sender identity is never a parameter of the browser surface: the
//! daemon derives `from` from the connection, so [`Lab`](crate::Lab)
//! derives it from the bound identity.

use serde::{Deserialize, Serialize};

use crate::agent_id::AgentId;

/// One letter on the wire (same fields as `prismattyc-mux::mailbox::Letter`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Letter {
    /// Store-assigned letter id (`msg:<20-digit seq>`).
    pub id: String,
    /// Sender's agent id. Connection-asserted in the daemon, never
    /// client-supplied.
    pub from: String,
    /// Recipient's agent id.
    pub to: String,
    /// One-line summary, delivered on `claim` with the body.
    pub summary: String,
    /// Full body, delivered on `claim` only.
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Open,
    Held,
}

#[derive(Clone, Debug)]
struct Stored {
    letter: Letter,
    state: State,
}

/// Process-local mailbox. Letters live in one `Vec` and every lookup is a
/// linear scan filtered on the recipient id; the daemon uses a SQLite
/// table indexed by recipient. Volumes here are a handful of letters.
#[derive(Debug, Default)]
pub struct Mailbox {
    seq: u64,
    letters: Vec<Stored>,
}

fn letter_id(seq: u64) -> String {
    format!("msg:{seq:020}")
}

/// Ids that do not parse as `msg:<seq>` are ignored by commit/release,
/// exactly as `store::set_state_or_delete` ignores them.
fn parse_letter_id(id: &str) -> Option<u64> {
    id.strip_prefix("msg:")?.parse().ok()
}

impl Mailbox {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn next_id(&mut self) -> String {
        self.seq = self.seq.saturating_add(1);
        letter_id(self.seq)
    }

    /// Queue a letter for `to`. Returns the assigned id and the
    /// recipient's **open** depth after the send (the depth readback the
    /// daemon answers `MailSend` with).
    pub fn send(
        &mut self,
        from: &AgentId,
        to: &AgentId,
        summary: &str,
        body: &str,
    ) -> (String, u32) {
        let id = self.next_id();
        self.letters.push(Stored {
            letter: Letter {
                id: id.clone(),
                from: from.as_str().to_string(),
                to: to.as_str().to_string(),
                summary: summary.to_string(),
                body: body.to_string(),
            },
            state: State::Open,
        });
        (id, self.depth(to).0)
    }

    /// Claim every uncommitted letter for `agent`: open letters become
    /// held, already-held letters are re-listed (the reconnect recovery
    /// path). Oldest first. Bodies are delivered here and nowhere else.
    pub fn claim(&mut self, agent: &AgentId) -> Vec<Letter> {
        let mut out = Vec::new();
        for s in &mut self.letters {
            if s.letter.to != agent.as_str() {
                continue;
            }
            s.state = State::Held;
            out.push(s.letter.clone());
        }
        out
    }

    /// Commit held letters **by id**: gone for good. Returns how many were
    /// actually committed; ids not held by this agent are ignored.
    pub fn commit(&mut self, agent: &AgentId, ids: &[String]) -> u32 {
        self.set_state_or_delete(agent, ids, None)
    }

    /// Release held letters **by id** back to open. Returns how many were
    /// actually released; ids not held by this agent are ignored.
    pub fn release(&mut self, agent: &AgentId, ids: &[String]) -> u32 {
        self.set_state_or_delete(agent, ids, Some(State::Open))
    }

    /// Shared commit/release: only letters this agent currently holds
    /// with a parseable id are affected.
    fn set_state_or_delete(
        &mut self,
        agent: &AgentId,
        ids: &[String],
        target: Option<State>,
    ) -> u32 {
        let mut affected = 0u32;
        for seq in ids.iter().filter_map(|id| parse_letter_id(id)) {
            let wanted = letter_id(seq);
            let Some(index) = self.letters.iter().position(|s| {
                s.letter.id == wanted && s.letter.to == agent.as_str() && s.state == State::Held
            }) else {
                continue;
            };
            match target {
                None => {
                    self.letters.remove(index);
                }
                Some(state) => self.letters[index].state = state,
            }
            affected = affected.saturating_add(1);
        }
        affected
    }

    /// `(open, held)` depth for `agent` — the peek the daemon answers
    /// `MailInbox` with. Counts only; no bodies.
    #[must_use]
    pub fn depth(&self, agent: &AgentId) -> (u32, u32) {
        let mut open = 0u32;
        let mut held = 0u32;
        for s in self
            .letters
            .iter()
            .filter(|s| s.letter.to == agent.as_str())
        {
            match s.state {
                State::Open => open = open.saturating_add(1),
                State::Held => held = held.saturating_add(1),
            }
        }
        (open, held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> AgentId {
        AgentId::new(name).unwrap()
    }

    fn send(m: &mut Mailbox, to: &AgentId, summary: &str) -> String {
        m.send(&agent("operator-b"), to, summary, "body").0
    }

    #[test]
    fn send_returns_id_and_open_depth_readback() {
        let mut m = Mailbox::new();
        let to = agent("operator-a");
        let (id, depth) = m.send(&agent("operator-b"), &to, "tests green", "all good");
        assert!(id.starts_with("msg:"));
        assert_eq!(depth, 1);
        let (_, depth) = m.send(&agent("operator-b"), &to, "second", "body");
        assert_eq!(depth, 2);
        assert_eq!(m.depth(&to), (2, 0));
        assert_eq!(m.depth(&agent("operator-b")), (0, 0));
    }

    #[test]
    fn depth_transitions_across_send_claim_commit() {
        let mut m = Mailbox::new();
        let to = agent("operator-a");
        assert_eq!(m.depth(&to), (0, 0));
        let id = send(&mut m, &to, "one");
        assert_eq!(m.depth(&to), (1, 0));

        let claimed = m.claim(&to);
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].summary, "one");
        assert_eq!(claimed[0].from, "operator-b");
        assert_eq!(m.depth(&to), (0, 1), "claimed is held, not gone");

        assert_eq!(m.commit(&to, &[id]), 1);
        assert_eq!(m.depth(&to), (0, 0));
        assert!(m.claim(&to).is_empty());
    }

    #[test]
    fn depth_transitions_across_send_claim_release() {
        let mut m = Mailbox::new();
        let to = agent("operator-a");
        let id = send(&mut m, &to, "one");
        m.claim(&to);
        assert_eq!(m.release(&to, std::slice::from_ref(&id)), 1);
        assert_eq!(m.depth(&to), (1, 0));
        assert_eq!(m.claim(&to).len(), 1, "reclaimable after release");
    }

    #[test]
    fn commit_is_id_scoped_and_held_letters_survive() {
        let mut m = Mailbox::new();
        let to = agent("operator-a");
        let first = send(&mut m, &to, "one");
        let second = send(&mut m, &to, "two");
        let third = send(&mut m, &to, "three");
        assert_eq!(m.claim(&to).len(), 3);
        assert_eq!(m.depth(&to), (0, 3));

        assert_eq!(m.commit(&to, &[second]), 1, "only the listed id commits");
        assert_eq!(m.depth(&to), (0, 2), "the other two stay held");

        let relisted = m.claim(&to);
        let ids: Vec<&str> = relisted.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, vec![first.as_str(), third.as_str()]);
    }

    #[test]
    fn foreign_or_unknown_ids_are_ignored() {
        let mut m = Mailbox::new();
        let to = agent("operator-a");
        let other = agent("operator-b");
        let id = send(&mut m, &to, "one");
        m.claim(&to);

        assert_eq!(m.commit(&other, std::slice::from_ref(&id)), 0);
        assert_eq!(m.release(&other, std::slice::from_ref(&id)), 0);
        assert_eq!(m.depth(&to), (0, 1));

        assert_eq!(m.commit(&to, &["msg:bogus".to_string()]), 0);
        assert_eq!(m.commit(&to, &["nonsense".to_string()]), 0);
        assert_eq!(m.commit(&to, &["msg:00000000000000009999".to_string()]), 0);
        assert_eq!(m.depth(&to), (0, 1));

        let mixed = vec!["msg:bogus".to_string(), id];
        assert_eq!(m.commit(&to, &mixed), 1, "known id still commits");
        assert_eq!(m.depth(&to), (0, 0));
    }

    #[test]
    fn open_letters_are_not_committable() {
        let mut m = Mailbox::new();
        let to = agent("operator-a");
        let id = send(&mut m, &to, "one");
        assert_eq!(m.commit(&to, std::slice::from_ref(&id)), 0);
        assert_eq!(m.release(&to, std::slice::from_ref(&id)), 0);
        assert_eq!(m.depth(&to), (1, 0), "commit does not skip the claim");
    }

    #[test]
    fn claim_does_not_reach_another_agents_mail() {
        let mut m = Mailbox::new();
        let to = agent("operator-a");
        let other = agent("operator-c");
        send(&mut m, &to, "one");
        assert!(m.claim(&other).is_empty());
        assert_eq!(m.depth(&to), (1, 0), "untouched by the other agent");
    }
}
