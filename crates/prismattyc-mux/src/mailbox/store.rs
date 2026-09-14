//! SQLite mailbox store for pmuxd (agent-keyed since).
//!
//! Letter send/claim/commit/release/broadcast.
//! Mail must survive pmuxd restarts.
//!
//! Session-as-seat replaces the old seat table: there are no seat
//! affinity rows here. Durable session-name → agent_id binding lives in
//! [`super::agents`]; liveness stays in the mux domain.
//!
//! The lifecycle contract:
//!
//! - letters are keyed by recipient agent id, never by session;
//! - **open** letters wait to be claimed; **held** letters are claimed
//!   but uncommitted;
//! - `claim` returns every uncommitted letter (open become held,
//!   already-held are re-listed — the reconnect recovery path);
//! - `commit` deletes held letters; `release` returns held to open.

use std::path::Path;

use super::AgentId;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, rusqlite::Error>;

/// One letter on the wire and in the store. `from`/`to` are agent ids;
/// `N@G` seat addressing is dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Letter {
    /// Daemon-assigned letter id.
    pub id: String,
    /// Sender's agent id (connection-asserted identity, never client-supplied
    /// on the `MailSend` wire shape).
    pub from: String,
    /// Recipient's agent id.
    pub to: String,
    /// One-line summary, delivered on `claim` with the body.
    pub summary: String,
    /// Full body, delivered on `claim`.
    pub body: String,
}

const LETTERS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS letters (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    recipient  TEXT NOT NULL,
    from_agent TEXT NOT NULL,
    to_agent   TEXT NOT NULL,
    summary    TEXT NOT NULL,
    body       TEXT NOT NULL,
    state      TEXT NOT NULL CHECK (state IN ('open', 'held'))
);
CREATE INDEX IF NOT EXISTS letters_recipient ON letters (recipient);
CREATE TABLE IF NOT EXISTS mailbox_forwards (
    old_address TEXT PRIMARY KEY,
    recipient TEXT NOT NULL
);
";

/// A SQLite-backed mailbox store. Not `Sync`; the daemon guards it
/// with a `Mutex` like any other shared state.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

fn letter_id(seq: i64) -> String {
    format!("msg:{seq:020}")
}

fn parse_letter_id(id: &str) -> Option<i64> {
    id.strip_prefix("msg:")?.parse().ok()
}

/// Default mailbox path: `$XDG_DATA_HOME/prismattyc/mail.db`, else
/// `$HOME/.local/share/prismattyc/mail.db`.
#[must_use]
pub fn default_mail_db_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".local/share"))
        });
    base.map_or_else(
        || std::path::PathBuf::from("prismattyc-mail.db"),
        |base| base.join("prismattyc").join("mail.db"),
    )
}

impl Store {
    /// Open (creating if needed) the store at `path`.
    /// Creates parent directories when missing.
    ///
    /// # Errors
    ///
    /// Propagates `SQLite` open and schema-migration errors.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
                return Err(rusqlite::Error::InvalidPath(parent.to_path_buf()));
            }
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// An in-memory store, for tests and ephemeral runs.
    ///
    /// # Errors
    ///
    /// Propagates `SQLite` schema-migration errors.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(LETTERS_SCHEMA)?;
        Ok(Self { conn })
    }

    /// Resolve a former mailbox address. Rename flattens every forwarding chain.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub fn resolve(&self, address: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT recipient FROM mailbox_forwards WHERE old_address = ?1",
                [address],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_else(|| address.to_string()))
    }

    /// List retained addresses for a mailbox.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub fn forwards(&self, address: &str) -> Result<Vec<String>> {
        self.conn.prepare("SELECT old_address FROM mailbox_forwards WHERE recipient = ?1 ORDER BY old_address")?
            .query_map([address], |row| row.get(0))?.collect()
    }

    /// List former and current addresses reserved by mailbox forwarding.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub fn reserved_addresses(&self) -> Result<Vec<String>> {
        self.conn.prepare("SELECT old_address FROM mailbox_forwards UNION SELECT recipient FROM mailbox_forwards")?
            .query_map([], |row| row.get(0))?.collect()
    }

    /// Move pending letters and retain old addresses in one durable transaction.
    /// Letter IDs, bodies, sender addresses, and open/held state stay intact.
    /// The control plane must validate ownership before calling this method.
    ///
    /// # Errors
    /// Propagates SQLite errors.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<()> {
        if old == new {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM mailbox_forwards WHERE old_address = ?1", [new])?;
        tx.execute(
            "UPDATE mailbox_forwards SET recipient = ?2 WHERE recipient = ?1",
            params![old, new],
        )?;
        tx.execute("INSERT INTO mailbox_forwards (old_address, recipient) VALUES (?1, ?2) ON CONFLICT(old_address) DO UPDATE SET recipient = excluded.recipient", params![old, new])?;
        tx.execute(
            "UPDATE letters SET recipient = ?2, to_agent = ?2 WHERE recipient = ?1",
            params![old, new],
        )?;
        tx.commit()
    }

    /// Queue a letter for `to`. Returns the letter id and the
    /// recipient's open depth after the send (the depth readback).
    ///
    /// The insert and the depth readback commit in one transaction:
    /// either the client gets its answer or nothing is persisted, so a
    /// `refused` send can never leave a duplicate behind on retry.
    ///
    /// # Errors
    ///
    /// Propagates `SQLite` errors.
    pub fn send(
        &mut self,
        from: &str,
        to: &AgentId,
        summary: &str,
        body: &str,
    ) -> Result<(String, u32)> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO letters (recipient, from_agent, to_agent, summary, body, state)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open')",
            params![to.as_str(), from, to.as_str(), summary, body],
        )?;
        let id = letter_id(tx.last_insert_rowid());
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM letters WHERE recipient = ?1 AND state = 'open'",
            params![to.as_str()],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok((id, u32::try_from(open).unwrap_or(u32::MAX)))
    }

    /// Insert one letter per recipient in a **single transaction** —
    /// either everyone gets it or nobody does (no persist-then-fail
    /// paths). Returns how many letters were inserted.
    ///
    /// # Errors
    ///
    /// Propagates `SQLite` errors; the transaction rolls back, so a
    /// failed broadcast delivers to nobody.
    pub fn broadcast(
        &mut self,
        from: &str,
        recipients: &[AgentId],
        summary: &str,
        body: &str,
    ) -> Result<u32> {
        let tx = self.conn.transaction()?;
        for agent in recipients {
            tx.execute(
                "INSERT INTO letters (recipient, from_agent, to_agent, summary, body, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'open')",
                params![agent.as_str(), from, agent.as_str(), summary, body],
            )?;
        }
        tx.commit()?;
        Ok(u32::try_from(recipients.len()).unwrap_or(u32::MAX))
    }

    /// Claim every uncommitted letter for `agent`: open letters become
    /// held, already-held letters are re-listed (the reconnect recovery
    /// path). Oldest first.
    ///
    /// # Errors
    ///
    /// Propagates `SQLite` errors.
    pub fn claim(&mut self, agent: &AgentId) -> Result<Vec<Letter>> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE letters SET state = 'held' WHERE recipient = ?1",
            params![agent.as_str()],
        )?;
        let mut stmt = tx.prepare(
            "SELECT seq, from_agent, to_agent, summary, body FROM letters
             WHERE recipient = ?1 ORDER BY seq",
        )?;
        let letters = stmt
            .query_map(params![agent.as_str()], |row| {
                Ok(Letter {
                    id: letter_id(row.get(0)?),
                    from: row.get(1)?,
                    to: row.get(2)?,
                    summary: row.get(3)?,
                    body: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        tx.commit()?;
        Ok(letters)
    }

    /// Commit held letters: gone for good. Returns how many were
    /// actually committed (ids not held by this agent are ignored).
    ///
    /// # Errors
    ///
    /// Propagates `SQLite` errors.
    pub fn commit(&mut self, agent: &AgentId, ids: &[String]) -> Result<u32> {
        self.set_state_or_delete(agent, ids, None)
    }

    /// Release held letters back to open. Returns how many were
    /// actually released.
    ///
    /// # Errors
    ///
    /// Propagates `SQLite` errors.
    pub fn release(&mut self, agent: &AgentId, ids: &[String]) -> Result<u32> {
        self.set_state_or_delete(agent, ids, Some("open"))
    }

    /// Shared commit/release: only letters this agent currently holds
    /// with a parseable id are affected.
    fn set_state_or_delete(
        &mut self,
        agent: &AgentId,
        ids: &[String],
        target: Option<&str>,
    ) -> Result<u32> {
        let mut affected = 0u32;
        let tx = self.conn.transaction()?;
        for seq in ids.iter().filter_map(|id| parse_letter_id(id)) {
            let n = match target {
                None => tx.execute(
                    "DELETE FROM letters WHERE recipient = ?1 AND seq = ?2 AND state = 'held'",
                    params![agent.as_str(), seq],
                )?,
                Some(state) => tx.execute(
                    "UPDATE letters SET state = ?3 WHERE recipient = ?1 AND seq = ?2 AND state = 'held'",
                    params![agent.as_str(), seq, state],
                )?,
            };
            affected += u32::try_from(n).unwrap_or(u32::MAX);
        }
        tx.commit()?;
        Ok(affected)
    }

    /// `(open, held)` depth for `agent`.
    ///
    /// # Errors
    ///
    /// Propagates `SQLite` errors.
    pub fn depth(&self, agent: &AgentId) -> Result<(u32, u32)> {
        let mut stmt = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM letters WHERE recipient = ?1 GROUP BY state")?;
        let mut open = 0u32;
        let mut held = 0u32;
        let rows = stmt.query_map(params![agent.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (state, count) = row?;
            let count = u32::try_from(count).unwrap_or(u32::MAX);
            match state.as_str() {
                "open" => open = count,
                "held" => held = count,
                _ => {}
            }
        }
        Ok((open, held))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> AgentId {
        AgentId::new(name).unwrap()
    }

    fn send(store: &mut Store, to: &AgentId, summary: &str) -> (String, u32) {
        store.send("operator-b", to, summary, "body").unwrap()
    }

    #[test]
    fn send_returns_id_and_depth_readback() {
        let mut store = Store::open_in_memory().unwrap();
        let (id1, d1) = send(&mut store, &agent("operator-a"), "one");
        let (id2, d2) = send(&mut store, &agent("operator-a"), "two");
        assert_ne!(id1, id2);
        assert_eq!((d1, d2), (1, 2));
        assert_eq!(store.depth(&agent("operator-a")).unwrap(), (2, 0));
    }

    #[test]
    fn rename_preserves_pending_delivery_and_flattens_old_addresses() {
        let mut store = Store::open_in_memory().unwrap();
        let old = agent("before");
        let renamed = agent("after");
        let (held_id, _) = store.send("sender", &old, "held", "first body").unwrap();
        store.claim(&old).unwrap();
        let (open_id, _) = store.send("sender", &old, "open", "second body").unwrap();
        store.rename("before", "after").unwrap();
        assert_eq!(store.depth(&old).unwrap(), (0, 0));
        assert_eq!(store.depth(&renamed).unwrap(), (1, 1));
        assert_eq!(store.resolve("before").unwrap(), "after");
        let letters = store.claim(&renamed).unwrap();
        assert_eq!(
            letters.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            [held_id.as_str(), open_id.as_str()]
        );
        assert_eq!(letters[0].from, "sender");
        assert_eq!(letters[0].body, "first body");
        assert_eq!(letters[1].body, "second body");
        assert!(letters.iter().all(|l| l.to == "after"));
        store.rename("after", "final").unwrap();
        assert_eq!(store.resolve("before").unwrap(), "final");
        assert_eq!(store.resolve("after").unwrap(), "final");
        // Reusing the original address must remove its old forward, not loop.
        store.rename("final", "before").unwrap();
        store.rename("before", "before").unwrap();
        assert_eq!(store.resolve("before").unwrap(), "before");
        assert_eq!(store.resolve("after").unwrap(), "before");
        assert_eq!(store.commit(&old, &[held_id, open_id]).unwrap(), 2);
        assert_eq!(store.depth(&old).unwrap(), (0, 0));
    }

    #[test]
    fn claim_holds_then_commit_removes() {
        let mut store = Store::open_in_memory().unwrap();
        let to = agent("operator-a");
        let (id, _) = send(&mut store, &to, "one");
        send(&mut store, &to, "two");

        let claimed = store.claim(&to).unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].summary, "one", "oldest first");
        assert_eq!(claimed[0].from, "operator-b");
        assert_eq!(claimed[0].to, "operator-a");
        assert_eq!(store.depth(&to).unwrap(), (0, 2));

        assert_eq!(store.commit(&to, &[id]).unwrap(), 1);
        assert_eq!(store.depth(&to).unwrap(), (0, 1));
    }

    #[test]
    fn reclaim_after_disconnect_recovers_held_letters() {
        let mut store = Store::open_in_memory().unwrap();
        let to = agent("operator-a");
        let (id, _) = send(&mut store, &to, "one");

        assert_eq!(store.claim(&to).unwrap().len(), 1);
        // ... client dies here, having stored nothing ...

        let second = store.claim(&to).unwrap();
        assert_eq!(
            second.iter().map(|e| &e.id).collect::<Vec<_>>(),
            vec![&id],
            "re-claim must re-list held letters"
        );
        assert_eq!(store.commit(&to, std::slice::from_ref(&id)).unwrap(), 1);
        assert!(
            store.claim(&to).unwrap().is_empty(),
            "committed letters stay gone"
        );
    }

    #[test]
    fn release_returns_to_open() {
        let mut store = Store::open_in_memory().unwrap();
        let to = agent("operator-a");
        let (id, _) = send(&mut store, &to, "one");
        assert_eq!(store.claim(&to).unwrap().len(), 1);

        assert_eq!(store.release(&to, &[id]).unwrap(), 1);
        assert_eq!(store.depth(&to).unwrap(), (1, 0));
        assert_eq!(
            store.claim(&to).unwrap().len(),
            1,
            "reclaimable after release"
        );
    }

    #[test]
    fn foreign_or_unknown_ids_are_ignored() {
        let mut store = Store::open_in_memory().unwrap();
        let to = agent("operator-a");
        let other = agent("operator-b");
        let (id, _) = send(&mut store, &to, "one");
        store.claim(&to).unwrap();

        assert_eq!(store.commit(&other, std::slice::from_ref(&id)).unwrap(), 0);
        assert_eq!(store.release(&other, std::slice::from_ref(&id)).unwrap(), 0);
        assert_eq!(store.depth(&to).unwrap(), (0, 1));

        assert_eq!(store.commit(&to, &["msg:bogus".to_string()]).unwrap(), 0);
        assert_eq!(store.depth(&to).unwrap(), (0, 1));
    }

    #[test]
    fn broadcast_delivers_one_letter_per_recipient() {
        let mut store = Store::open_in_memory().unwrap();
        let recipients = vec![agent("operator-a"), agent("operator-b")];
        let delivered = store
            .broadcast("operator-b", &recipients, "standup in 5", "body")
            .unwrap();
        assert_eq!(delivered, 2);
        for name in ["operator-a", "operator-b"] {
            let claimed = store.claim(&agent(name)).unwrap();
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].summary, "standup in 5");
            assert_eq!(claimed[0].from, "operator-b");
        }
        // Empty fan-out is a no-op, not an error.
        let delivered = store
            .broadcast("operator-b", &[], "nobody home", "body")
            .unwrap();
        assert_eq!(delivered, 0);
    }

    #[test]
    fn recipients_with_mail_listing_was_considered() {
        // MailWho lists live agent-bound sessions only (design: an agent
        // is not listed until its session exists), so the store exposes
        // no recipient-listing helper. Queued mail for offline agents is
        // still durable and claimable after that agent binds a session.
        let mut store = Store::open_in_memory().unwrap();
        send(&mut store, &agent("offline-agent"), "queued");
        assert_eq!(store.depth(&agent("offline-agent")).unwrap(), (1, 0));
    }

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pmux-mailbox-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("mail.db")
    }

    #[test]
    fn letters_survive_reopen() {
        // The whole point of the store: mail outlives the process.
        let path = temp_db("open-reopen");

        let id = {
            let mut store = Store::open(&path).unwrap();
            let (id, _) = send(&mut store, &agent("operator-a"), "durable");
            id
        }; // store dropped = "daemon restart"

        let mut store = Store::open(&path).unwrap();
        let claimed = store.claim(&agent("operator-a")).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, id);
        assert_eq!(claimed[0].summary, "durable");

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn held_letters_survive_reopen_and_relist() {
        // Claim, then "restart" before commit: the held letter must
        // still be there, and claim must re-list it (the recovery path
        // works across restarts too, not just reconnects).
        let path = temp_db("held-reopen");

        let id = {
            let mut store = Store::open(&path).unwrap();
            let (id, _) = send(&mut store, &agent("operator-a"), "held");
            assert_eq!(store.claim(&agent("operator-a")).unwrap().len(), 1);
            id
        };

        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.depth(&agent("operator-a")).unwrap(), (0, 1));
        let claimed = store.claim(&agent("operator-a")).unwrap();
        assert_eq!(claimed.len(), 1, "held letter re-listed after reopen");
        assert_eq!(claimed[0].id, id);
        assert_eq!(store.commit(&agent("operator-a"), &[id]).unwrap(), 1);
        assert_eq!(store.depth(&agent("operator-a")).unwrap(), (0, 0));

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn default_mail_db_path_uses_xdg_data_home() {
        let prior = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", "/tmp/pmux-xdg-data");
        let path = default_mail_db_path();
        match prior {
            Some(value) => std::env::set_var("XDG_DATA_HOME", value),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert_eq!(
            path,
            std::path::PathBuf::from("/tmp/pmux-xdg-data/prismattyc/mail.db")
        );
    }
}
