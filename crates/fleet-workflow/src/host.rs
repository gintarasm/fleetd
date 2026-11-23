//! What a workflow may call: the [`Host`] trait, shaped like the M5
//! `workflow` WIT world's imports, and the [`Storage`] it owns.
//!
//! Async methods return boxed futures, so `dyn Host` works (a WASM host is
//! another implementation, behind the same pointer).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use fleet_proto::{AgentInfo, Forgotten, SpawnAgent};

use crate::error::ActionError;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type HostResult<T> = Result<T, ActionError>;
pub type Labels = HashMap<String, String>;

/// Who invoked the action (ADR 0072): the user, or an agent, whose scope
/// is its descendants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    User,
    Agent(u64),
}

/// Runs a program on the node and waits for it (M4 spec §1, `Exec`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    /// Absolute.
    pub cwd: String,
    /// Added to the node's login-shell environment (or, with `clear_env`,
    /// the whole environment).
    pub env: Labels,
    pub clear_env: bool,
    pub stdin: Option<Vec<u8>>,
    /// 0: the node's default (10 minutes).
    pub timeout_ms: u64,
    /// 0: the node's cap (15 MiB, stdout and stderr together).
    pub max_output: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    /// `None` when killed by a signal or the timeout.
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadFileRequest {
    /// Absolute.
    pub path: String,
    pub offset: u64,
    /// 0: the node's cap (15 MiB).
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileData {
    pub data: Vec<u8>,
    pub size: u64,
    pub mtime_unix_ms: i64,
    pub eof: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListDirRequest {
    /// Absolute.
    pub path: String,
    /// 0: the node's cap (10,000).
    pub max_entries: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    #[default]
    Other,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_unix_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
    pub truncated: bool,
}

/// What a workflow may call. One host serves one action call of one
/// workflow: it knows the caller and the workflow (its storage, its owned
/// label prefix). Everything acts as the caller: spawns are the caller's
/// spawns (parent, narrowed labels, quotas, approval), lists and forgets
/// stay in the caller's scope.
pub trait Host: Send + Sync {
    /// Who invoked the action.
    fn caller(&self) -> Caller;
    /// `Exec` on the node (capability `exec:<bin>`).
    fn exec(&self, req: ExecRequest) -> BoxFuture<'_, HostResult<ExecOutput>>;
    /// `ReadFile` on the node (capability `fs:read:<prefix>`).
    fn read_file(&self, req: ReadFileRequest) -> BoxFuture<'_, HostResult<FileData>>;
    /// `ListDir` on the node (capability `fs:read:<prefix>`).
    fn list_dir(&self, req: ListDirRequest) -> BoxFuture<'_, HostResult<DirListing>>;
    /// Starts an agent as the caller (capability `spawn`). Keys of the
    /// workflow's owned label prefix replace inherited ones (ADR 0092).
    fn spawn(&self, req: SpawnAgent) -> BoxFuture<'_, HostResult<AgentInfo>>;
    /// Whether the caller may spawn one more agent now (the quotas), as a
    /// dry run before the workflow makes side effects.
    fn check_spawn(&self) -> BoxFuture<'_, HostResult<()>>;
    /// The agents in the caller's scope whose labels contain `selector`.
    fn list_agents(&self, selector: Labels) -> BoxFuture<'_, HostResult<Vec<AgentInfo>>>;
    /// Sets and removes labels of an agent in the caller's scope; only keys
    /// of the workflow's owned prefix (capability `labels:<prefix>`).
    fn set_labels(
        &self,
        agent: u64,
        set: Labels,
        remove: Vec<String>,
    ) -> BoxFuture<'_, HostResult<()>>;
    /// Forgets exited agents in the caller's scope (capability `spawn`).
    fn forget(&self, ids: Vec<u64>) -> BoxFuture<'_, HostResult<Forgotten>>;
    /// The workflow's own storage (capability `storage:own`).
    fn storage(&self) -> &dyn Storage;
}

/// A workflow's key-value storage (ADR 0092: `workflow_kv`, 64 KiB per
/// value, 10,000 keys per workflow).
pub trait Storage: Send + Sync {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, HostResult<Option<Vec<u8>>>>;
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, HostResult<()>>;
    /// Whether the key was there.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, HostResult<bool>>;
    /// The keys starting with `prefix`, sorted.
    fn keys<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, HostResult<Vec<String>>>;
}

/// A reference to a host is a host (so the daemon can hand one out per
/// call without giving it away).
impl<H: Host + ?Sized> Host for &H {
    fn caller(&self) -> Caller {
        (**self).caller()
    }
    fn exec(&self, req: ExecRequest) -> BoxFuture<'_, HostResult<ExecOutput>> {
        (**self).exec(req)
    }
    fn read_file(&self, req: ReadFileRequest) -> BoxFuture<'_, HostResult<FileData>> {
        (**self).read_file(req)
    }
    fn list_dir(&self, req: ListDirRequest) -> BoxFuture<'_, HostResult<DirListing>> {
        (**self).list_dir(req)
    }
    fn spawn(&self, req: SpawnAgent) -> BoxFuture<'_, HostResult<AgentInfo>> {
        (**self).spawn(req)
    }
    fn check_spawn(&self) -> BoxFuture<'_, HostResult<()>> {
        (**self).check_spawn()
    }
    fn list_agents(&self, selector: Labels) -> BoxFuture<'_, HostResult<Vec<AgentInfo>>> {
        (**self).list_agents(selector)
    }
    fn set_labels(
        &self,
        agent: u64,
        set: Labels,
        remove: Vec<String>,
    ) -> BoxFuture<'_, HostResult<()>> {
        (**self).set_labels(agent, set, remove)
    }
    fn forget(&self, ids: Vec<u64>) -> BoxFuture<'_, HostResult<Forgotten>> {
        (**self).forget(ids)
    }
    fn storage(&self) -> &dyn Storage {
        (**self).storage()
    }
}
