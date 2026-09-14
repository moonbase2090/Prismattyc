use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable logical identity of an agent, e.g. `operator-a`.
///
/// Format: lowercase ASCII alphanumerics and hyphens, 2–64 chars, no
/// leading/trailing or doubled hyphens. A common convention is
/// `<agent>-<slug>`, but any string passing validation is allowed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId(String);

impl AgentId {
    /// Validate and wrap a raw identity string.
    ///
    /// # Errors
    ///
    /// Returns [`AgentIdError`] describing the first violated rule.
    pub fn new(raw: impl Into<String>) -> Result<Self, AgentIdError> {
        let raw = raw.into();
        if raw.len() < 2 {
            return Err(AgentIdError::TooShort);
        }
        if raw.len() > 64 {
            return Err(AgentIdError::TooLong);
        }
        if let Some(bad) = raw
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return Err(AgentIdError::InvalidChar(bad));
        }
        if raw.starts_with('-') || raw.ends_with('-') {
            return Err(AgentIdError::EdgeHyphen);
        }
        if raw.contains("--") {
            return Err(AgentIdError::DoubledHyphen);
        }
        Ok(Self(raw))
    }

    /// The identity as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<AgentId> for String {
    fn from(id: AgentId) -> Self {
        id.0
    }
}

impl TryFrom<String> for AgentId {
    type Error = AgentIdError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

/// Why an [`AgentId`] rejected its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentIdError {
    /// Fewer than 2 characters.
    TooShort,
    /// More than 64 characters.
    TooLong,
    /// A character outside `[a-z0-9-]`.
    InvalidChar(char),
    /// Leading or trailing hyphen.
    EdgeHyphen,
    /// Two consecutive hyphens.
    DoubledHyphen,
}

impl fmt::Display for AgentIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => f.write_str("agent id must be at least 2 characters"),
            Self::TooLong => f.write_str("agent id must be at most 64 characters"),
            Self::InvalidChar(c) => {
                write!(
                    f,
                    "agent id contains invalid character {c:?} (want [a-z0-9-])"
                )
            }
            Self::EdgeHyphen => f.write_str("agent id must not start or end with a hyphen"),
            Self::DoubledHyphen => f.write_str("agent id must not contain consecutive hyphens"),
        }
    }
}

impl std::error::Error for AgentIdError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_fabric_ids() {
        for ok in ["operator-a", "operator-b", "operator-c", "a1", "x-y-z-9"] {
            assert!(AgentId::new(ok).is_ok(), "should accept {ok}");
        }
    }

    #[test]
    fn rejects_malformed_ids() {
        assert_eq!(AgentId::new("x"), Err(AgentIdError::TooShort));
        assert_eq!(AgentId::new("x".repeat(65)), Err(AgentIdError::TooLong));
        assert_eq!(
            AgentId::new("Operator-A"),
            Err(AgentIdError::InvalidChar('O'))
        );
        assert_eq!(AgentId::new("-operator-a"), Err(AgentIdError::EdgeHyphen));
        assert_eq!(AgentId::new("operator-a-"), Err(AgentIdError::EdgeHyphen));
        assert_eq!(
            AgentId::new("operator--a"),
            Err(AgentIdError::DoubledHyphen)
        );
        assert_eq!(
            AgentId::new("operator_a"),
            Err(AgentIdError::InvalidChar('_'))
        );
    }

    #[test]
    fn serde_roundtrip() {
        let id = AgentId::new("operator-a").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"operator-a\"");
        assert_eq!(serde_json::from_str::<AgentId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<AgentId>("\"BAD ID\"").is_err());
    }
}
