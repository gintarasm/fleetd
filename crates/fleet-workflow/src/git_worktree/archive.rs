//! `git-worktree.archive` and `git-worktree.prune`.

use crate::error::ActionError;
use crate::registry::{ActionCall, ActionOutcome};

use super::GitWorktree;
use super::git::Git;

pub async fn run(_git: &Git<'_>, _call: &ActionCall) -> Result<ActionOutcome, ActionError> {
    Err(ActionError::failed("todo"))
}

pub async fn prune(
    _flow: &GitWorktree,
    _git: &Git<'_>,
    _call: &ActionCall,
) -> Result<ActionOutcome, ActionError> {
    Err(ActionError::failed("todo"))
}
