//! How an action fails when it does not return an outcome.

use std::fmt;

/// The prefix of a refusal by scope or capability; the CLI exits 3 for it
/// (as `fleet_node::scope::FORBIDDEN_PREFIX`).
pub const FORBIDDEN_PREFIX: &str = "forbidden:";

/// An action's error: not overridable (an overridable refusal is an
/// outcome, `ActionRefused`). Sent to the client as an `Error` whose
/// message is this error's `Display`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionError {
    /// Scope, a capability, or `callers`/`user_only`: `forbidden: …`.
    Forbidden(String),
    /// A bad call: unknown action or param, wrong kind, missing param.
    Invalid(String),
    /// The action ran and failed.
    Failed(String),
    /// The caller's connection closed (reported at the next host call).
    Cancelled,
}

impl ActionError {
    pub fn forbidden(what: impl Into<String>) -> Self {
        ActionError::Forbidden(what.into())
    }
    pub fn invalid(what: impl Into<String>) -> Self {
        ActionError::Invalid(what.into())
    }
    pub fn failed(what: impl Into<String>) -> Self {
        ActionError::Failed(what.into())
    }
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::Forbidden(m) => write!(f, "{FORBIDDEN_PREFIX} {m}"),
            ActionError::Invalid(m) | ActionError::Failed(m) => f.write_str(m),
            ActionError::Cancelled => f.write_str("cancelled: the caller went away"),
        }
    }
}

impl std::error::Error for ActionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_errors_read_like_the_cores() {
        assert_eq!(
            ActionError::forbidden("agents cannot prune").to_string(),
            "forbidden: agents cannot prune"
        );
        assert_eq!(ActionError::invalid("no param x").to_string(), "no param x");
    }
}
