//! Workflows (ADR 0025, 0090, 0092): named actions with parameter schemas
//! that every client calls the same way, through `ListActions` and
//! `RunAction`.
//!
//! This crate is the line the M5 WASM plugin host keeps: a workflow is an
//! `impl` [`Workflow`], and [`Host`] is what it may call, shaped like the
//! future `workflow` WIT world's imports. `fleetd` owns the [`Registry`]
//! and the `Host` that reaches the node; this crate knows nothing of the
//! node. Everything a workflow may do is checked here, for built-ins too:
//! its [`Manifest`]'s capabilities ([`checked`]), and every call's params
//! against the action's schema and the caller's scope ([`params`]).

pub mod checked;
pub mod error;
#[cfg(test)]
mod fake;
pub mod git_worktree;
pub mod host;
pub mod manifest;
pub mod params;
pub mod registry;

pub use checked::{CancelFlag, Checked};
pub use error::ActionError;
pub use host::{
    BoxFuture, Caller, DirEntry, DirListing, EntryKind, ExecOutput, ExecRequest, FileData, Host,
    HostResult, Labels, ListDirRequest, ReadFileRequest, Storage,
};
pub use manifest::{Capability, Manifest};
pub use params::{Params, validate};
pub use registry::{ActionCall, ActionOutcome, Registry, Workflow, WorkflowInfo};
