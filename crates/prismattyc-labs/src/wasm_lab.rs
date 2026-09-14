//! `wasm_bindgen` browser surface (wasm32 only).
//!
//! The mail verbs mirror the `pmuxd` control plane: a connection is bound
//! to one agent identity, and every verb acts on that identity only.
//! `mailSend` derives `from` from the binding — there is no way to forge a
//! sender — and claim/commit/release/peek take no agent argument, because
//! the daemon's `Mail*` requests take none either. Switch seats with
//! `setIdentity`, which is the browser analogue of opening a second
//! connection, not of reading someone else's mail.

use crate::agent_id::AgentId;
use crate::mailbox::{Letter, Mailbox};
use prismattyc_core::splash::{
    art_frame, art_frame_width, FRAME_MS, HOLD_FRAME_MS, HOLD_MS, INTRO_MS, LOOP_MS, REFLECT_MS,
    SETTLED_MS,
};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// Browser-facing lab handle. One instance is one connection: it owns the
/// in-memory mailbox and is bound to exactly one agent identity.
#[wasm_bindgen]
pub struct Lab {
    agent: AgentId,
    mail: Mailbox,
}

#[derive(Serialize)]
struct SplashMeta {
    cols: usize,
    intro_ms: u64,
    loop_ms: u64,
    settled_ms: u64,
    frame_ms: u64,
    hold_frame_ms: u64,
    hold_ms: u64,
    reflect_ms: u64,
}

/// Flat splash cell for JS: `[row, col, ch, r, g, b]`, converted with
/// `serde_wasm_bindgen` (native JS values, no JSON round trip).
#[derive(Serialize)]
struct SplashFrame {
    rows: usize,
    cols: usize,
    /// Sparse cells that are not blank space.
    cells: Vec<(usize, usize, char, u8, u8, u8)>,
}

/// `{ id, depth }` — the id assigned to the letter and the recipient's
/// open depth after the send, matching `MailSent`.
#[derive(Serialize)]
struct MailSent {
    id: String,
    depth: u32,
}

/// `{ open, held }` — the two-tier depth peek, matching `MailDepth`.
#[derive(Serialize)]
struct MailDepth {
    open: u32,
    held: u32,
}

fn parse_agent(raw: &str) -> Result<AgentId, JsValue> {
    AgentId::new(raw).map_err(|error| JsValue::from_str(&error.to_string()))
}

#[wasm_bindgen]
impl Lab {
    /// Bind a lab to one agent identity. Rejects ids `pmuxd` would reject,
    /// with the same message.
    ///
    /// # Errors
    ///
    /// Returns the `AgentId` validation error text when `agent` is malformed.
    #[wasm_bindgen(constructor)]
    pub fn new(agent: &str) -> Result<Lab, JsValue> {
        Ok(Lab {
            agent: parse_agent(agent)?,
            mail: Mailbox::new(),
        })
    }

    /// The bound agent identity — the `from` of everything this lab sends.
    #[wasm_bindgen(js_name = identity)]
    #[must_use]
    pub fn identity(&self) -> String {
        self.agent.as_str().to_string()
    }

    /// Rebind to another identity, keeping the mailbox. This is a seat
    /// switch (a new connection), not cross-agent access.
    ///
    /// # Errors
    ///
    /// Returns the `AgentId` validation error text when `agent` is malformed.
    #[wasm_bindgen(js_name = setIdentity)]
    pub fn set_identity(&mut self, agent: &str) -> Result<(), JsValue> {
        self.agent = parse_agent(agent)?;
        Ok(())
    }

    /// Timing constants for the splash attract loop.
    ///
    /// # Errors
    ///
    /// Returns a serialization error string if the meta cannot be converted.
    #[wasm_bindgen(js_name = splashMeta)]
    pub fn splash_meta(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&SplashMeta {
            cols: art_frame_width(),
            intro_ms: INTRO_MS,
            loop_ms: LOOP_MS,
            settled_ms: SETTLED_MS,
            frame_ms: FRAME_MS,
            hold_frame_ms: HOLD_FRAME_MS,
            hold_ms: HOLD_MS,
            reflect_ms: REFLECT_MS,
        })
        .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Sample `art_frame(elapsed_ms)` as sparse colored cells.
    ///
    /// Takes `f64` so callers can pass `performance.now()` directly;
    /// negative and non-finite inputs clamp to 0.
    ///
    /// # Errors
    ///
    /// Returns a serialization error string if the frame cannot be converted.
    #[wasm_bindgen(js_name = splashFrame)]
    pub fn splash_frame(&self, elapsed_ms: f64) -> Result<JsValue, JsValue> {
        let elapsed_ms = if elapsed_ms.is_finite() && elapsed_ms > 0.0 {
            elapsed_ms as u64
        } else {
            0
        };
        let frame = art_frame(elapsed_ms);
        let rows = frame.len();
        let cols = art_frame_width();
        let mut cells = Vec::new();
        for (r, row) in frame.iter().enumerate() {
            for (c, (ch, rgb)) in row.iter().enumerate() {
                if *ch == ' ' {
                    continue;
                }
                cells.push((r, c, *ch, rgb[0], rgb[1], rgb[2]));
            }
        }
        serde_wasm_bindgen::to_value(&SplashFrame { rows, cols, cells })
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Send a letter as the bound identity. Returns `{ id, depth }`.
    ///
    /// # Errors
    ///
    /// Returns the `AgentId` validation error text when `to` is malformed.
    #[wasm_bindgen(js_name = mailSend)]
    pub fn mail_send(&mut self, to: &str, summary: &str, body: &str) -> Result<JsValue, JsValue> {
        let to = AgentId::new(to)
            .map_err(|error| JsValue::from_str(&format!("recipient {to:?}: {error}")))?;
        let (id, depth) = self.mail.send(&self.agent, &to, summary, body);
        serde_wasm_bindgen::to_value(&MailSent { id, depth })
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Peek at this agent's mailbox depth: `{ open, held }`. Counts only —
    /// bodies arrive on `mailClaim` and nowhere else.
    ///
    /// # Errors
    ///
    /// Returns a serialization error string if the depth cannot be converted.
    #[wasm_bindgen(js_name = mailPeek)]
    pub fn mail_peek(&self) -> Result<JsValue, JsValue> {
        let (open, held) = self.mail.depth(&self.agent);
        serde_wasm_bindgen::to_value(&MailDepth { open, held })
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Claim every uncommitted letter for this agent (open become held,
    /// already-held are re-listed). The only path to letter bodies.
    ///
    /// # Errors
    ///
    /// Returns a serialization error string if the letters cannot be converted.
    #[wasm_bindgen(js_name = mailClaim)]
    pub fn mail_claim(&mut self) -> Result<JsValue, JsValue> {
        to_letters(self.mail.claim(&self.agent))
    }

    /// Commit the listed held letters. Returns how many were committed;
    /// ids this agent does not hold are ignored.
    #[wasm_bindgen(js_name = mailCommit)]
    pub fn mail_commit(&mut self, ids: Vec<String>) -> u32 {
        self.mail.commit(&self.agent, &ids)
    }

    /// Release the listed held letters back to open. Returns how many were
    /// released; ids this agent does not hold are ignored.
    #[wasm_bindgen(js_name = mailRelease)]
    pub fn mail_release(&mut self, ids: Vec<String>) -> u32 {
        self.mail.release(&self.agent, &ids)
    }
}

fn to_letters(letters: Vec<Letter>) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(&letters).map_err(|e| JsValue::from_str(&e.to_string()))
}
