//! `git-worktree.diff`.

use crate::error::ActionError;
use crate::registry::{ActionCall, ActionOutcome};

use super::git::Git;

pub async fn run(_git: &Git<'_>, _call: &ActionCall) -> Result<ActionOutcome, ActionError> {
    Err(ActionError::failed("todo"))
}
