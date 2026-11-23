//! The host a workflow actually gets: every call checked against its
//! [`Manifest`] before it reaches the real host, and cancelled once the
//! caller's connection closes (ADR 0092).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fleet_proto::{AgentInfo, Forgotten, SpawnAgent};

use crate::error::ActionError;
use crate::host::{
    BoxFuture, Caller, DirListing, ExecOutput, ExecRequest, FileData, Host, HostResult, Labels,
    ListDirRequest, ReadFileRequest, Storage,
};
use crate::manifest::Manifest;

/// Set when the caller's connection closes; the action sees it at its next
/// host call.
#[derive(Clone, Debug, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Cancellation as the workflow sees it: once cancelled, its next host call
/// fails with [`ActionError::Cancelled`]. Calls after that run again, so
/// the workflow can roll back what it did (remove a worktree it just made),
/// except those that start something new: `spawn` and `check_spawn` stay
/// cancelled.
struct Gate {
    cancel: CancelFlag,
    reported: AtomicBool,
}

impl Gate {
    fn check(&self, starts_something: bool) -> HostResult<()> {
        if !self.cancel.is_cancelled() {
            return Ok(());
        }
        if !self.reported.swap(true, Ordering::SeqCst) || starts_something {
            return Err(ActionError::Cancelled);
        }
        Ok(())
    }
}

fn refused<'a, T: Send + 'a>(e: ActionError) -> BoxFuture<'a, HostResult<T>> {
    Box::pin(std::future::ready(Err(e)))
}

/// `inner`, held to `manifest` and to `cancel`.
pub struct Checked<'a> {
    inner: &'a dyn Host,
    manifest: &'a Manifest,
    gate: Arc<Gate>,
    storage: CheckedStorage<'a>,
}

impl<'a> Checked<'a> {
    pub fn new(inner: &'a dyn Host, manifest: &'a Manifest, cancel: CancelFlag) -> Self {
        let gate = Arc::new(Gate {
            cancel,
            reported: AtomicBool::new(false),
        });
        Checked {
            inner,
            manifest,
            storage: CheckedStorage {
                inner: inner.storage(),
                allowed: manifest.allows_storage(),
                gate: Arc::clone(&gate),
            },
            gate,
        }
    }

    /// The gate, then the capability (`Err` is the refusal's reason).
    fn admit(&self, starts_something: bool, allowed: Result<(), String>) -> HostResult<()> {
        self.gate.check(starts_something)?;
        allowed.map_err(ActionError::Forbidden)
    }

    fn need_spawn(&self) -> Result<(), String> {
        if self.manifest.allows_spawn() {
            Ok(())
        } else {
            Err("the workflow did not declare the capability `spawn`".into())
        }
    }

    fn need_read(&self, path: &str) -> Result<(), String> {
        if self.manifest.allows_read(path) {
            Ok(())
        } else {
            Err(format!("the workflow may not read {path:?}"))
        }
    }
}

impl Host for Checked<'_> {
    fn caller(&self) -> Caller {
        self.inner.caller()
    }

    fn exec(&self, req: ExecRequest) -> BoxFuture<'_, HostResult<ExecOutput>> {
        let Some(argv0) = req.argv.first() else {
            return refused(ActionError::invalid("exec: argv is empty"));
        };
        let allowed = if self.manifest.allows_exec(argv0) {
            Ok(())
        } else {
            Err(format!(
                "the workflow may not run {argv0:?} (no `exec:{argv0}`)"
            ))
        };
        match self.admit(false, allowed) {
            Ok(()) => self.inner.exec(req),
            Err(e) => refused(e),
        }
    }

    fn read_file(&self, req: ReadFileRequest) -> BoxFuture<'_, HostResult<FileData>> {
        match self.admit(false, self.need_read(&req.path)) {
            Ok(()) => self.inner.read_file(req),
            Err(e) => refused(e),
        }
    }

    fn list_dir(&self, req: ListDirRequest) -> BoxFuture<'_, HostResult<DirListing>> {
        match self.admit(false, self.need_read(&req.path)) {
            Ok(()) => self.inner.list_dir(req),
            Err(e) => refused(e),
        }
    }

    fn spawn(&self, req: SpawnAgent) -> BoxFuture<'_, HostResult<AgentInfo>> {
        match self.admit(true, self.need_spawn()) {
            Ok(()) => self.inner.spawn(req),
            Err(e) => refused(e),
        }
    }

    fn check_spawn(&self) -> BoxFuture<'_, HostResult<()>> {
        match self.admit(true, self.need_spawn()) {
            Ok(()) => self.inner.check_spawn(),
            Err(e) => refused(e),
        }
    }

    fn list_agents(&self, selector: Labels) -> BoxFuture<'_, HostResult<Vec<AgentInfo>>> {
        match self.admit(false, Ok(())) {
            Ok(()) => self.inner.list_agents(selector),
            Err(e) => refused(e),
        }
    }

    fn set_labels(
        &self,
        agent: u64,
        set: Labels,
        remove: Vec<String>,
    ) -> BoxFuture<'_, HostResult<()>> {
        let outside = set
            .keys()
            .chain(&remove)
            .find(|k| !self.manifest.allows_label(k));
        let allowed = match outside {
            None => Ok(()),
            Some(key) => Err(format!(
                "label {key:?} is not in the workflow's own prefix ({})",
                self.manifest.owned_prefix().unwrap_or("none")
            )),
        };
        match self.admit(false, allowed) {
            Ok(()) => self.inner.set_labels(agent, set, remove),
            Err(e) => refused(e),
        }
    }

    fn forget(&self, ids: Vec<u64>) -> BoxFuture<'_, HostResult<Forgotten>> {
        match self.admit(false, self.need_spawn()) {
            Ok(()) => self.inner.forget(ids),
            Err(e) => refused(e),
        }
    }

    fn storage(&self) -> &dyn Storage {
        &self.storage
    }
}

/// The workflow's storage, behind `storage:own` and the gate.
struct CheckedStorage<'a> {
    inner: &'a dyn Storage,
    allowed: bool,
    gate: Arc<Gate>,
}

impl CheckedStorage<'_> {
    fn admit(&self) -> HostResult<()> {
        self.gate.check(false)?;
        if self.allowed {
            Ok(())
        } else {
            Err(ActionError::forbidden(
                "the workflow did not declare the capability `storage:own`",
            ))
        }
    }
}

impl Storage for CheckedStorage<'_> {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, HostResult<Option<Vec<u8>>>> {
        match self.admit() {
            Ok(()) => self.inner.get(key),
            Err(e) => refused(e),
        }
    }
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, HostResult<()>> {
        match self.admit() {
            Ok(()) => self.inner.put(key, value),
            Err(e) => refused(e),
        }
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, HostResult<bool>> {
        match self.admit() {
            Ok(()) => self.inner.delete(key),
            Err(e) => refused(e),
        }
    }
    fn keys<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, HostResult<Vec<String>>> {
        match self.admit() {
            Ok(()) => self.inner.keys(prefix),
            Err(e) => refused(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeHost;

    fn manifest(caps: &[&str]) -> Manifest {
        Manifest::parse(caps).unwrap()
    }

    fn exec(argv: &[&str]) -> ExecRequest {
        ExecRequest {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: "/".into(),
            ..Default::default()
        }
    }

    fn is_forbidden<T: std::fmt::Debug>(r: HostResult<T>) -> bool {
        matches!(r, Err(ActionError::Forbidden(_)))
    }

    #[tokio::test]
    async fn exec_and_reads_need_their_capabilities() {
        let fake = FakeHost::new(Caller::User);
        let m = manifest(&["exec:git", "fs:read:/repo"]);
        let host = Checked::new(&fake, &m, CancelFlag::default());
        assert!(host.exec(exec(&["git", "status"])).await.is_ok());
        assert!(is_forbidden(host.exec(exec(&["sh", "-c", "x"])).await));
        assert!(matches!(
            host.exec(exec(&[])).await,
            Err(ActionError::Invalid(_))
        ));
        let read = |p: &str| ReadFileRequest {
            path: p.into(),
            ..Default::default()
        };
        assert!(host.read_file(read("/repo/a")).await.is_ok());
        assert!(is_forbidden(host.read_file(read("/etc/passwd")).await));
        assert!(is_forbidden(
            host.read_file(read("/repo/../etc/passwd")).await
        ));
        let ls = |p: &str| ListDirRequest {
            path: p.into(),
            ..Default::default()
        };
        assert!(host.list_dir(ls("/repo")).await.is_ok());
        assert!(is_forbidden(host.list_dir(ls("/")).await));
        assert_eq!(
            fake.calls(),
            ["exec git status", "read /repo/a", "ls /repo"]
        );
    }

    #[tokio::test]
    async fn spawn_forget_labels_and_storage_need_theirs() {
        let fake = FakeHost::new(Caller::Agent(1));
        let none = manifest(&[]);
        let host = Checked::new(&fake, &none, CancelFlag::default());
        assert_eq!(host.caller(), Caller::Agent(1));
        assert!(is_forbidden(host.spawn(SpawnAgent::default()).await));
        assert!(is_forbidden(host.check_spawn().await));
        assert!(is_forbidden(host.forget(vec![2]).await));
        let set: Labels = [("git.repo".to_string(), "/r".to_string())].into();
        assert!(is_forbidden(host.set_labels(2, set.clone(), vec![]).await));
        assert!(is_forbidden(host.storage().get("k").await));
        assert!(is_forbidden(host.storage().put("k", vec![]).await));
        assert!(is_forbidden(host.storage().keys("").await));
        assert!(is_forbidden(host.storage().delete("k").await));
        // Listing the caller's scope needs nothing.
        assert!(host.list_agents(Labels::new()).await.is_ok());
        assert_eq!(fake.calls(), ["list"]);

        let all = manifest(&["spawn", "labels:git.", "storage:own"]);
        let host = Checked::new(&fake, &all, CancelFlag::default());
        assert!(host.check_spawn().await.is_ok());
        assert!(host.spawn(SpawnAgent::default()).await.is_ok());
        assert!(host.forget(vec![2]).await.is_ok());
        assert!(
            host.set_labels(2, set, vec!["git.base".into()])
                .await
                .is_ok()
        );
        let outside: Labels = [("project".to_string(), "x".to_string())].into();
        assert!(is_forbidden(host.set_labels(2, outside, vec![]).await));
        assert!(is_forbidden(
            host.set_labels(2, Labels::new(), vec!["name".into()]).await
        ));
        host.storage().put("k", b"v".to_vec()).await.unwrap();
        assert_eq!(host.storage().get("k").await.unwrap(), Some(b"v".to_vec()));
        assert_eq!(host.storage().keys("").await.unwrap(), ["k"]);
        assert!(host.storage().delete("k").await.unwrap());
    }

    #[tokio::test]
    async fn a_cancel_fails_the_next_call_then_allows_rollback_but_no_spawn() {
        let fake = FakeHost::new(Caller::User);
        let m = manifest(&["exec:git", "spawn", "storage:own"]);
        let cancel = CancelFlag::default();
        let host = Checked::new(&fake, &m, cancel.clone());
        assert!(host.exec(exec(&["git", "a"])).await.is_ok());
        cancel.cancel();
        assert!(cancel.is_cancelled());
        assert_eq!(
            host.exec(exec(&["git", "b"])).await,
            Err(ActionError::Cancelled)
        );
        // The workflow now rolls back: plain calls run again...
        assert!(
            host.exec(exec(&["git", "worktree", "remove"]))
                .await
                .is_ok()
        );
        assert!(host.storage().delete("wt/x").await.is_ok());
        // ...but nothing new starts.
        assert_eq!(host.check_spawn().await, Err(ActionError::Cancelled));
        assert_eq!(
            host.spawn(SpawnAgent::default()).await.map(|_| ()),
            Err(ActionError::Cancelled)
        );
        assert_eq!(fake.calls(), ["exec git a", "exec git worktree remove"]);
    }

    #[tokio::test]
    async fn a_cancel_before_storage_is_reported_by_storage() {
        let fake = FakeHost::new(Caller::User);
        let m = manifest(&["storage:own"]);
        let cancel = CancelFlag::default();
        let host = Checked::new(&fake, &m, cancel.clone());
        cancel.cancel();
        assert_eq!(host.storage().get("k").await, Err(ActionError::Cancelled));
        assert_eq!(host.storage().get("k").await, Ok(None));
    }
}
