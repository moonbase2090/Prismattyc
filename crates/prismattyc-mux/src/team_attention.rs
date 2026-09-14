//! Explicit requests for human input. Mail delivery remains independent.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRequest {
    pub pane_id: u64,
    pub revision: u64,
    pub message: String,
    pub source: String,
    pub raised_at_ms: u64,
    pub snoozed_until_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AttentionAction {
    Resolve,
    Snooze { seconds: u32 },
}

impl AttentionRequest {
    pub fn needs_input(&self, now_ms: u64) -> bool {
        self.snoozed_until_ms <= now_ms
    }
}
