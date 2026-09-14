//! Durable agent mailbox inside pmuxd.
//!
//! SQLite store, Mail* dispatch, and in-process doorbell.

pub mod agent_id;
pub mod agents;
pub mod store;
pub mod watch;

pub use agent_id::{AgentId, AgentIdError};
pub use store::{default_mail_db_path, Letter, Store};
pub use watch::MailboxWatch;
