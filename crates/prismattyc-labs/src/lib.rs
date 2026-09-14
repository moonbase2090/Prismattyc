//! Prismattyc browser labs (PT-106 POC).
//!
//! Pure client WASM: in-memory agent mail + splash `art_frame` from core.
//! No PTYs, sockets, or pmuxd.
//!
//! This crate is a **teaching mirror** of the real pmux mailbox. Its
//! lifecycle, identity binding, and id-scoped commit/release must match
//! `prismattyc-mux::mailbox::store` and the `Mail*` handlers in
//! `prismattyc-mux::control`; if those change, change this too (PT-117).
//!
//! Native builds compile the mailbox and the agent-id validator (and their
//! unit tests) only. The `wasm_bindgen` `Lab` surface is gated to
//! `target_arch = "wasm32"`; CI checks it with
//! `cargo check -p prismattyc-labs --target wasm32-unknown-unknown`.
//!
//! Build the website package with:
//! `wasm-pack build crates/prismattyc-labs --target web --out-dir <site>/labs/pkg`.
//! The UI itself lives in the separate `prismattyc-website` repository.

pub mod agent_id;
pub mod mailbox;

pub use agent_id::{AgentId, AgentIdError};
pub use mailbox::{Letter, Mailbox};

#[cfg(target_arch = "wasm32")]
mod wasm_lab;

#[cfg(target_arch = "wasm32")]
pub use wasm_lab::Lab;
