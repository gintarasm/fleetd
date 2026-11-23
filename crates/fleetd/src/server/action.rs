//! Workflows in the server (ADR 0092, M4 spec §2): `ListActions`,
//! `RunAction`, and the [`Host`] that reaches the node for a workflow.
//!
//! The registry is a static list of built-in workflows, built on the first
//! `ListActions` or `RunAction`, never at startup. Each `RunAction` runs on
//! a task of its own, as the caller, so a slow action does not block the
//! connection (like `Wait`): at most [`MAX_PER_CONNECTION`] per connection
//! and [`MAX_PER_DAEMON`] per daemon run at once, more wait their turn.
//! The task is not a request in flight (it never holds off a restart);
//! each host call that changes the node is admitted on its own instead, and
//! refused while fleetd is restarting. Closing the connection cancels the
//! action at its next host call ([`CancelFlag`]), and withdraws a spawn
//! waiting for approval; the task is not aborted, so the workflow can roll
//! back what it did.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fleet_node::approval as node_approval;
use fleet_node::lease::now_ms;
use fleet_node::node::Node;
use fleet_node::primitives;
use fleet_node::scope::{Caller as NodeCaller, FORBIDDEN_PREFIX, forbidden};
use fleet_proto::{
    ActionList, ActionResult, AgentInfo, Forgotten, Frame, Response, RunAction, SpawnAgent,
    response,
};
use fleet_workflow::{
    ActionError, BoxFuture, Caller, CancelFlag, DirEntry, DirListing, EntryKind, ExecOutput,
    ExecRequest, FileData, Host, HostResult, Labels, ListDirRequest, ReadFileRequest, Registry,
    Storage, Workflow,
};
use tokio::sync::{Semaphore, mpsc, watch};
use tracing::{info, warn};

use super::{Shared, error};
use crate::approval::MAX_PENDING_PER_AGENT;

/// Actions one connection runs at once; more wait.
pub(super) const MAX_PER_CONNECTION: usize = 4;
/// Actions the daemon runs at once; more wait.
pub(super) const MAX_PER_DAEMON: usize = 16;

/// The built-in workflows (M4: native only; git-worktree, ADR 0093, joins
/// here).
fn builtin_workflows() -> Vec<Arc<dyn Workflow>> {
    Vec::new()
}

/// The daemon's workflows: the registry (built lazily) and the daemon-wide
/// limit.
pub(super) struct Actions {
    registry: OnceLock<Result<Registry, String>>,
    slots: Arc<Semaphore>,
}

impl Actions {
    pub(super) fn new() -> Actions {
        Actions {
            registry: OnceLock::new(),
            slots: Arc::new(Semaphore::new(MAX_PER_DAEMON)),
        }
    }

    /// The registry; `Err` (logged once) if a built-in is malformed, which
    /// a test catches before it ships.
    fn registry(&self) -> Result<&Registry, String> {
        self.registry
            .get_or_init(|| {
                let built = Registry::new(builtin_workflows());
                if let Err(e) = &built {
                    warn!(error = %e, "the workflow registry is malformed");
                }
                built
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Uses `workflows` instead of the built-ins (before the first use).
    #[cfg(test)]
    pub(super) fn preset(&self, workflows: Vec<Arc<dyn Workflow>>) {
        let set = self.registry.set(Registry::new(workflows));
        assert!(set.is_ok(), "the registry was already built");
    }
}

/// A connection's actions: its limit, and its end, which cancels them.
pub(super) struct ConnActions {
    slots: Arc<Semaphore>,
    cancel: CancelFlag,
    /// Never sent on: dropped with the connection, which wakes the hosts'
    /// receivers (a spawn waiting for approval is then withdrawn).
    closed: watch::Sender<()>,
}

impl ConnActions {
    pub(super) fn new() -> ConnActions {
        ConnActions {
            slots: Arc::new(Semaphore::new(MAX_PER_CONNECTION)),
            cancel: CancelFlag::default(),
            closed: watch::channel(()).0,
        }
    }
}

impl Drop for ConnActions {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn to_caller(c: NodeCaller) -> Caller {
    match c {
        NodeCaller::User => Caller::User,
        NodeCaller::Agent(a) => Caller::Agent(a),
    }
}

/// An error of the node's as the workflow sees it: `forbidden:` stays a
/// refusal.
fn from_io(e: &io::Error) -> ActionError {
    from_message(e.to_string())
}

fn from_message(m: String) -> ActionError {
    match m.strip_prefix(FORBIDDEN_PREFIX) {
        Some(rest) => ActionError::Forbidden(rest.trim_start().to_owned()),
        None => ActionError::Failed(m),
    }
}

fn restarting() -> ActionError {
    ActionError::failed("fleetd is restarting")
}

/// Answers `ListActions`: the actions the caller may call.
pub(super) fn list(shared: &Shared, caller: NodeCaller) -> response::Kind {
    match shared.actions.registry() {
        Ok(r) => response::Kind::ActionList(Box::new(ActionList {
            actions: r.list(to_caller(caller)),
        })),
        Err(e) => error(format!("workflows unavailable: {e}")),
    }
}

/// Everything a `RunAction` task needs from its connection.
pub(super) struct Start {
    pub(super) node: Arc<Node>,
    pub(super) shared: Arc<Shared>,
    pub(super) caller: NodeCaller,
    pub(super) outbox: mpsc::Sender<Frame>,
    pub(super) conn: u64,
}

/// Starts `req` (request `id`) on a task of its own; it answers on the
/// outbox when done.
pub(super) fn start(s: Start, actions: &ConnActions, id: u64, req: RunAction) {
    let conn_slots = Arc::clone(&actions.slots);
    let cancel = actions.cancel.clone();
    let closed = actions.closed.subscribe();
    tokio::spawn(async move {
        let kind = run(&s, conn_slots, cancel, closed, req).await;
        let frame = Frame::Response(Response {
            id,
            kind: Some(kind),
        });
        // A closed connection no longer wants the answer.
        let _ = s.outbox.send(frame).await;
    });
}

async fn run(
    s: &Start,
    conn_slots: Arc<Semaphore>,
    cancel: CancelFlag,
    closed: watch::Receiver<()>,
    req: RunAction,
) -> response::Kind {
    let registry = match s.shared.actions.registry() {
        Ok(r) => r,
        Err(e) => return error(format!("workflows unavailable: {e}")),
    };
    // Per connection first, so one connection's queue cannot hold daemon
    // slots it is not using.
    let Ok(_conn_slot) = conn_slots.acquire_owned().await else {
        return error("fleetd is stopping".into());
    };
    let Ok(_daemon_slot) = Arc::clone(&s.shared.actions.slots).acquire_owned().await else {
        return error("fleetd is stopping".into());
    };
    let name = req.name.clone();
    info!(conn = s.conn, action = ?name, "action started");
    let outcome = registry
        .run(req, cancel, |w| NodeHost {
            node: Arc::clone(&s.node),
            shared: Arc::clone(&s.shared),
            caller: s.caller,
            owned_prefix: w.owned_prefix.clone(),
            closed: closed.clone(),
            storage: NodeStorage {
                node: Arc::clone(&s.node),
                shared: Arc::clone(&s.shared),
                workflow: w.id.clone(),
            },
        })
        .await;
    match outcome {
        Ok(outcome) => {
            info!(conn = s.conn, action = ?name, "action finished");
            response::Kind::ActionResult(Box::new(ActionResult {
                outcome: Some(outcome),
            }))
        }
        Err(e) => {
            info!(conn = s.conn, action = ?name, error = %e, "action failed");
            error(e.to_string())
        }
    }
}

/// Runs `f` on the blocking pool; a node error becomes the workflow's.
async fn blocking<T: Send + 'static>(
    node: &Arc<Node>,
    f: impl FnOnce(&Node) -> io::Result<T> + Send + 'static,
) -> HostResult<T> {
    let node = Arc::clone(node);
    match tokio::task::spawn_blocking(move || f(&node)).await {
        Ok(r) => r.map_err(|e| from_io(&e)),
        Err(e) => Err(ActionError::failed(e.to_string())),
    }
}

/// [`blocking`], admitted as a request that changes the node (refused
/// while fleetd is restarting, and holding a restart off while it runs).
async fn admitted<T: Send + 'static>(
    shared: &Arc<Shared>,
    node: &Arc<Node>,
    f: impl FnOnce(&Node) -> io::Result<T> + Send + 'static,
) -> HostResult<T> {
    let Some(_admitted) = shared.admit() else {
        return Err(restarting());
    };
    blocking(node, f).await
}

/// The node, as one workflow's host for one call of `caller`'s.
struct NodeHost {
    node: Arc<Node>,
    shared: Arc<Shared>,
    caller: NodeCaller,
    owned_prefix: Option<String>,
    closed: watch::Receiver<()>,
    storage: NodeStorage,
}

fn to_exec(req: ExecRequest) -> primitives::ExecRequest {
    primitives::ExecRequest {
        argv: req.argv,
        cwd: PathBuf::from(req.cwd),
        env: req.env.into_iter().collect(),
        clear_env: req.clear_env,
        stdin: req.stdin.unwrap_or_default(),
        timeout: Duration::from_millis(req.timeout_ms),
        max_output: usize::try_from(req.max_output).unwrap_or(usize::MAX),
    }
}

fn from_exec(r: primitives::ExecResult) -> ExecOutput {
    ExecOutput {
        // A signal or the timeout ended it: no exit code.
        exit_code: (r.signal == 0 && !r.timed_out).then_some(r.exit_code),
        stdout: r.stdout,
        stderr: r.stderr,
        timed_out: r.timed_out,
        stdout_truncated: r.stdout_truncated,
        stderr_truncated: r.stderr_truncated,
    }
}

fn from_listing(l: primitives::DirListing) -> DirListing {
    DirListing {
        entries: l
            .entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                kind: match e.kind {
                    primitives::EntryKind::File => EntryKind::File,
                    primitives::EntryKind::Dir => EntryKind::Dir,
                    primitives::EntryKind::Symlink => EntryKind::Symlink,
                    primitives::EntryKind::Other => EntryKind::Other,
                },
                size: e.size,
                mtime_unix_ms: e.mtime_unix_ms,
            })
            .collect(),
        truncated: l.truncated,
    }
}

impl NodeHost {
    /// Waits for the user's approval of an agent's spawn (ADR 0079), as a
    /// `Spawn` request does: the quotas and labels are checked first, then
    /// the request waits, and is withdrawn if the connection closes.
    async fn approve(&self, a: u64, req: &SpawnAgent, timeout_secs: u32) -> HostResult<()> {
        let caller = self.caller;
        let spawn = req.clone();
        let owned = self.owned_prefix.clone();
        let mut pending = blocking(&self.node, move |n| match &owned {
            Some(prefix) => n.precheck_spawn_for_workflow(caller, &spawn, prefix),
            None => n.precheck_spawn(caller, &spawn),
        })
        .await?;
        let approvals = self.node.approvals();
        if approvals.list().iter().filter(|p| p.parent == a).count() >= MAX_PENDING_PER_AGENT {
            return Err(from_io(&forbidden(format!(
                "{MAX_PENDING_PER_AGENT} spawns of agent {a} already wait for approval"
            ))));
        }
        let now = now_ms();
        pending.asked_unix_ms = now;
        pending.expires_unix_ms = now.saturating_add(i64::from(timeout_secs) * 1000);
        let mut ticket = approvals.ask(pending);
        info!(
            agent = a,
            pending = ticket.id(),
            "an action's spawn waits for approval"
        );
        let mut closed = self.closed.clone();
        let wait = Duration::from_secs(u64::from(timeout_secs));
        let decided = tokio::select! {
            r = tokio::time::timeout(wait, ticket.decision()) => r.ok(),
            _ = closed.changed() => {
                // Dropping the ticket withdraws the request.
                drop(ticket);
                return Err(ActionError::Cancelled);
            }
        };
        let decision = match decided {
            Some(d) => d,
            None => ticket.expire(),
        };
        match node_approval::refusal(decision, timeout_secs) {
            None => Ok(()),
            Some(message) => Err(from_message(message)),
        }
    }
}

impl Host for NodeHost {
    fn caller(&self) -> Caller {
        to_caller(self.caller)
    }

    fn exec(&self, req: ExecRequest) -> BoxFuture<'_, HostResult<ExecOutput>> {
        // The node's own Exec (ADR 0091): its concurrency limit, process
        // group and timeout; dropping the future (the action's task ends)
        // kills the child.
        let env = self.node.login_env();
        Box::pin(async move {
            primitives::exec(to_exec(req), env)
                .await
                .map(from_exec)
                .map_err(|e| from_io(&e))
        })
    }

    fn read_file(&self, req: ReadFileRequest) -> BoxFuture<'_, HostResult<FileData>> {
        Box::pin(blocking(&self.node, move |_| {
            let d = primitives::read_file(&primitives::ReadFileRequest {
                path: PathBuf::from(req.path),
                offset: req.offset,
                max_bytes: usize::try_from(req.max_bytes).unwrap_or(usize::MAX),
            })?;
            Ok(FileData {
                data: d.data,
                size: d.size,
                mtime_unix_ms: d.mtime_unix_ms,
                eof: d.eof,
            })
        }))
    }

    fn list_dir(&self, req: ListDirRequest) -> BoxFuture<'_, HostResult<DirListing>> {
        Box::pin(blocking(&self.node, move |_| {
            primitives::list_dir(&primitives::ListDirRequest {
                path: PathBuf::from(req.path),
                max_entries: req.max_entries as usize,
            })
            .map(from_listing)
        }))
    }

    fn spawn(&self, req: SpawnAgent) -> BoxFuture<'_, HostResult<AgentInfo>> {
        Box::pin(async move {
            if let (NodeCaller::Agent(a), Some(timeout)) = (self.caller, self.node.spawn_approval())
            {
                self.approve(a, &req, timeout).await?;
            }
            let caller = self.caller;
            let owned = self.owned_prefix.clone();
            let info = admitted(&self.shared, &self.node, move |n| match &owned {
                Some(prefix) => n.spawn_for_workflow(caller, req, prefix),
                None => n.spawn_by(caller, req),
            })
            .await?;
            info!(agent = info.id, "spawned by an action");
            Ok(info)
        })
    }

    fn check_spawn(&self) -> BoxFuture<'_, HostResult<()>> {
        let caller = self.caller;
        Box::pin(blocking(&self.node, move |n| n.check_spawn(caller)))
    }

    fn list_agents(&self, selector: Labels) -> BoxFuture<'_, HostResult<Vec<AgentInfo>>> {
        let caller = self.caller;
        Box::pin(blocking(&self.node, move |n| n.list_for(caller, &selector)))
    }

    fn set_labels(
        &self,
        agent: u64,
        set: Labels,
        remove: Vec<String>,
    ) -> BoxFuture<'_, HostResult<()>> {
        let caller = self.caller;
        Box::pin(admitted(&self.shared, &self.node, move |n| {
            if let NodeCaller::Agent(a) = caller
                && !n.tree()?.is_descendant(a, agent)
            {
                return Err(forbidden(format!(
                    "agent {agent} is not a descendant of agent {a}"
                )));
            }
            n.set_labels(agent, set, &remove)
        }))
    }

    fn forget(&self, ids: Vec<u64>) -> BoxFuture<'_, HostResult<Forgotten>> {
        let caller = self.caller;
        Box::pin(admitted(&self.shared, &self.node, move |n| {
            n.forget_by(caller, &ids, false, &Labels::new(), None)
                .map(|o| Forgotten {
                    ids: o.forgotten,
                    refused: o.refused,
                })
                .map_err(|e| e.error)
        }))
    }

    fn storage(&self) -> &dyn Storage {
        &self.storage
    }
}

/// A workflow's storage in the node's `workflow_kv` table.
struct NodeStorage {
    node: Arc<Node>,
    shared: Arc<Shared>,
    workflow: String,
}

impl Storage for NodeStorage {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, HostResult<Option<Vec<u8>>>> {
        let (w, k) = (self.workflow.clone(), key.to_owned());
        Box::pin(blocking(&self.node, move |n| n.kv_get(&w, &k)))
    }
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, HostResult<()>> {
        let (w, k) = (self.workflow.clone(), key.to_owned());
        Box::pin(admitted(&self.shared, &self.node, move |n| {
            n.kv_put(&w, &k, &value)
        }))
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, HostResult<bool>> {
        let (w, k) = (self.workflow.clone(), key.to_owned());
        Box::pin(admitted(&self.shared, &self.node, move |n| {
            n.kv_delete(&w, &k)
        }))
    }
    fn keys<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, HostResult<Vec<String>>> {
        let (w, p) = (self.workflow.clone(), prefix.to_owned());
        Box::pin(blocking(&self.node, move |n| n.kv_keys(&w, &p)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use fleet_node::scope::{PARENT_LABEL, Quotas};
    use fleet_proto::action_result::Outcome;
    use fleet_proto::param_value::Value;
    use fleet_proto::{
        ActionCallers, ActionDone, ActionSpawned, ActionSpec, ListActions, ParamKind, ParamSpec,
        ParamValue, Request, StringMap, request,
    };
    use fleet_workflow::{ActionCall, ActionOutcome, Manifest};

    use super::*;
    use crate::scope::Scope;
    use crate::server::Connection;
    use crate::server::tests::{next_response, test_server_conn};

    /// A workflow for tests (id `t`, owning `t.`): its actions say what
    /// they got and what the host did.
    struct TestFlow {
        /// `block` waits for a permit here.
        gate: Arc<Semaphore>,
        running: AtomicUsize,
        most_running: AtomicUsize,
        /// What `block`'s host call after the gate returned.
        after_gate: Mutex<Vec<String>>,
        /// The workflow's `fs:read:` prefix.
        read_root: String,
    }

    impl Default for TestFlow {
        fn default() -> Self {
            TestFlow {
                gate: Arc::new(Semaphore::new(0)),
                running: AtomicUsize::new(0),
                most_running: AtomicUsize::new(0),
                after_gate: Mutex::new(Vec::new()),
                read_root: "/nowhere".into(),
            }
        }
    }

    fn spec(name: &str, callers: ActionCallers, params: &[(&str, ParamKind)]) -> ActionSpec {
        ActionSpec {
            name: format!("t.{name}"),
            callers: callers as i32,
            params: params
                .iter()
                .map(|(k, kind)| ParamSpec {
                    key: (*k).into(),
                    kind: *kind as i32,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn done(message: String) -> ActionOutcome {
        Outcome::Done(ActionDone { message })
    }

    impl Workflow for TestFlow {
        fn id(&self) -> &str {
            "t"
        }
        fn manifest(&self) -> Result<Manifest, String> {
            let read = format!("fs:read:{}", self.read_root);
            Manifest::parse(&[
                "exec:git",
                "exec:sh",
                "spawn",
                "labels:t.",
                "storage:own",
                &read,
            ])
        }
        fn actions(&self) -> Vec<ActionSpec> {
            use ActionCallers::{User, UserAndAgents as Both};
            vec![
                spec("echo", Both, &[("agent", ParamKind::ParamAgent)]),
                spec("mine", User, &[]),
                spec("spawn", Both, &[("labels", ParamKind::ParamLabels)]),
                spec("block", Both, &[]),
                spec("store", Both, &[]),
                spec("exec", Both, &[("argv", ParamKind::ParamArgv)]),
                spec("read", Both, &[("path", ParamKind::ParamPath)]),
                spec("ls", Both, &[("path", ParamKind::ParamPath)]),
            ]
        }
        fn run<'a>(
            &'a self,
            host: &'a dyn Host,
            call: ActionCall,
        ) -> BoxFuture<'a, Result<ActionOutcome, ActionError>> {
            Box::pin(async move {
                match call.action.as_str() {
                    "echo" | "mine" => Ok(done(format!(
                        "{:?} {:?} {}",
                        host.caller(),
                        call.params.agent("agent"),
                        call.cwd
                    ))),
                    "spawn" => {
                        let mut labels = call.params.map("labels").cloned().unwrap_or_default();
                        labels.insert("t.where".into(), "/wt/new".into());
                        let agent = host
                            .spawn(SpawnAgent {
                                program: "/bin/sh".into(),
                                args: vec!["-c".into(), "exec sleep 600".into()],
                                labels,
                                ..Default::default()
                            })
                            .await?;
                        Ok(Outcome::Spawned(Box::new(ActionSpawned {
                            agent: Some(agent),
                            message: "started".into(),
                        })))
                    }
                    "block" => {
                        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                        self.most_running.fetch_max(now, Ordering::SeqCst);
                        let permit = self.gate.acquire().await;
                        permit.unwrap().forget();
                        self.running.fetch_sub(1, Ordering::SeqCst);
                        let got = host.storage().get("k").await;
                        self.after_gate.lock().unwrap().push(format!("{got:?}"));
                        got.map(|_| done("unblocked".into()))
                    }
                    "store" => {
                        host.storage().put("k", b"v".to_vec()).await?;
                        let v = host.storage().get("k").await?.unwrap_or_default();
                        Ok(done(String::from_utf8_lossy(&v).into_owned()))
                    }
                    "exec" => {
                        let out = host
                            .exec(ExecRequest {
                                argv: call.params.list("argv").unwrap_or_default().to_vec(),
                                cwd: "/".into(),
                                env: [("T_VAR".to_string(), "set".to_string())].into(),
                                timeout_ms: 5_000,
                                ..Default::default()
                            })
                            .await?;
                        Ok(done(format!(
                            "{:?} {:?} {:?} {}",
                            out.exit_code,
                            String::from_utf8_lossy(&out.stdout),
                            String::from_utf8_lossy(&out.stderr),
                            out.timed_out
                        )))
                    }
                    "read" => {
                        let d = host
                            .read_file(ReadFileRequest {
                                path: call.params.string("path").unwrap_or_default().into(),
                                offset: 1,
                                max_bytes: 3,
                            })
                            .await?;
                        Ok(done(format!(
                            "{} {} {}",
                            String::from_utf8_lossy(&d.data),
                            d.size,
                            d.eof
                        )))
                    }
                    "ls" => {
                        let l = host
                            .list_dir(ListDirRequest {
                                path: call.params.string("path").unwrap_or_default().into(),
                                max_entries: 0,
                            })
                            .await?;
                        let mut names: Vec<String> = l
                            .entries
                            .iter()
                            .map(|e| format!("{}:{:?}", e.name, e.kind))
                            .collect();
                        names.sort();
                        Ok(done(names.join(" ")))
                    }
                    other => Err(ActionError::invalid(format!("no {other}"))),
                }
            })
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        node: Arc<Node>,
        flow: Arc<TestFlow>,
        conn: Connection,
        rx: mpsc::Receiver<Frame>,
        _server: super::super::Server,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.node.shutdown();
        }
    }

    /// A user's connection to a node with the test workflow.
    fn fixture(quotas: Quotas) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(Node::open(dir.path()).unwrap().with_quotas(quotas));
        let (server, conn, rx) = test_server_conn(Arc::clone(&node));
        let flow = Arc::new(TestFlow::default());
        server.shared.actions.preset(vec![flow.clone()]);
        Fixture {
            _dir: dir,
            node,
            flow,
            conn,
            rx,
            _server: server,
        }
    }

    fn as_agent(f: &mut Fixture, a: u64) {
        f.conn.caller = NodeCaller::Agent(a);
        f.conn.scope = Scope::new(&f.node, f.conn.caller);
    }

    fn sleeper(labels: &[(&str, &str)]) -> SpawnAgent {
        SpawnAgent {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exec sleep 600".into()],
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            ..Default::default()
        }
    }

    fn run_req(name: &str, params: Vec<(&str, Value)>) -> request::Kind {
        request::Kind::RunAction(Box::new(RunAction {
            name: name.into(),
            params: params
                .into_iter()
                .map(|(k, v)| (k.to_string(), ParamValue { value: Some(v) }))
                .collect(),
            cwd: "/here".into(),
        }))
    }

    async fn call(f: &mut Fixture, id: u64, kind: request::Kind) -> response::Kind {
        assert!(
            f.conn
                .handle(Request {
                    id,
                    kind: Some(kind)
                })
                .await
        );
        let r = next_response(&mut f.rx).await;
        assert_eq!(r.id, id);
        r.kind.unwrap()
    }

    fn outcome(kind: response::Kind) -> Outcome {
        match kind {
            response::Kind::ActionResult(r) => r.outcome.unwrap(),
            other => panic!("{other:?}"),
        }
    }

    fn error_text(kind: response::Kind) -> String {
        match kind {
            response::Kind::Error(e) => e.message,
            other => panic!("{other:?}"),
        }
    }

    async fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn names(kind: response::Kind) -> Vec<String> {
        match kind {
            response::Kind::ActionList(l) => l.actions.into_iter().map(|a| a.name).collect(),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn agents_list_only_the_actions_open_to_them() {
        let mut f = fixture(Quotas::default());
        let all = names(call(&mut f, 1, request::Kind::ListActions(ListActions {})).await);
        assert!(all.contains(&"t.mine".to_string()), "{all:?}");
        let a = f.node.spawn(sleeper(&[])).unwrap().id;
        as_agent(&mut f, a);
        let theirs = names(call(&mut f, 2, request::Kind::ListActions(ListActions {})).await);
        assert_eq!(theirs.len(), all.len() - 1);
        assert!(!theirs.contains(&"t.mine".to_string()));
    }

    #[tokio::test]
    async fn the_built_in_registry_is_well_formed() {
        assert!(Registry::new(builtin_workflows()).is_ok());
    }

    #[tokio::test]
    async fn an_action_runs_as_its_caller_with_its_cwd() {
        let mut f = fixture(Quotas::default());
        let kid = f.node.spawn(sleeper(&[("name", "kid")])).unwrap().id;
        let got = outcome(
            call(
                &mut f,
                1,
                run_req("t.echo", vec![("agent", Value::String("kid".into()))]),
            )
            .await,
        );
        assert_eq!(got, done(format!("User Some({kid}) /here")));
        let e = error_text(call(&mut f, 2, run_req("t.nope", vec![])).await);
        assert!(e.contains("no action"), "{e}");
    }

    #[tokio::test]
    async fn an_agent_is_held_to_its_scope() {
        let mut f = fixture(Quotas::default());
        let a = f.node.spawn(sleeper(&[])).unwrap().id;
        let other = f.node.spawn(sleeper(&[])).unwrap().id;
        let kid = f
            .node
            .spawn_by(NodeCaller::Agent(a), sleeper(&[]))
            .unwrap()
            .id;
        as_agent(&mut f, a);
        let got = outcome(
            call(
                &mut f,
                1,
                run_req("t.echo", vec![("agent", Value::Int(kid as i64))]),
            )
            .await,
        );
        assert_eq!(got, done(format!("Agent({a}) Some({kid}) /here")));
        let e = error_text(
            call(
                &mut f,
                2,
                run_req("t.echo", vec![("agent", Value::Int(other as i64))]),
            )
            .await,
        );
        assert!(e.starts_with("forbidden:"), "{e}");
        let e = error_text(call(&mut f, 3, run_req("t.mine", vec![])).await);
        assert!(e.starts_with("forbidden:"), "{e}");
    }

    #[tokio::test]
    async fn an_agents_action_spawns_its_child_with_the_workflows_labels() {
        let mut f = fixture(Quotas::default());
        let a = f
            .node
            .spawn(sleeper(&[("project", "p"), ("t.where", "/wt/parent")]))
            .unwrap()
            .id;
        as_agent(&mut f, a);
        let labels = Value::Map(StringMap {
            entries: HashMap::from([("role".to_string(), "rev".to_string())]),
        });
        let Outcome::Spawned(s) =
            outcome(call(&mut f, 1, run_req("t.spawn", vec![("labels", labels)])).await)
        else {
            panic!("not spawned");
        };
        let child = s.agent.unwrap();
        assert_eq!(child.labels.get(PARENT_LABEL), Some(&a.to_string()));
        assert_eq!(child.labels.get("project").map(String::as_str), Some("p"));
        assert_eq!(child.labels.get("role").map(String::as_str), Some("rev"));
        assert_eq!(
            child.labels.get("t.where").map(String::as_str),
            Some("/wt/new"),
            "the workflow's own key replaced the inherited one"
        );
        // The agent cannot put the workflow's prefix in its own labels.
        let sneaky = Value::Map(StringMap {
            entries: HashMap::from([("t.where".to_string(), "/etc".to_string())]),
        });
        let e = error_text(call(&mut f, 2, run_req("t.spawn", vec![("labels", sneaky)])).await);
        assert!(e.starts_with("forbidden:"), "{e}");
    }

    #[tokio::test]
    async fn an_agents_action_spawn_is_under_its_quota() {
        let mut f = fixture(Quotas {
            max_children: 0,
            ..Quotas::default()
        });
        let a = f.node.spawn(sleeper(&[])).unwrap().id;
        as_agent(&mut f, a);
        let e = error_text(call(&mut f, 1, run_req("t.spawn", vec![])).await);
        assert!(
            e.starts_with("forbidden:") && e.contains("max_children"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn an_agents_action_spawn_waits_for_approval() {
        let mut f = fixture(Quotas {
            spawn_approval: true,
            spawn_approval_timeout_secs: 60,
            ..Quotas::default()
        });
        let a = f
            .node
            .spawn(sleeper(&[("t.where", "/wt/parent")]))
            .unwrap()
            .id;
        as_agent(&mut f, a);
        assert!(
            f.conn
                .handle(Request {
                    id: 1,
                    kind: Some(run_req("t.spawn", vec![]))
                })
                .await
        );
        let node = Arc::clone(&f.node);
        wait_until(|| !node.approvals().list().is_empty()).await;
        let pending = f.node.approvals().list().remove(0);
        assert_eq!(pending.parent, a);
        assert_eq!(
            pending.labels.get("t.where").map(String::as_str),
            Some("/wt/new")
        );
        f.node.approvals().decide(pending.id, false).unwrap();
        let e = error_text(next_response(&mut f.rx).await.kind.unwrap());
        assert!(
            e.starts_with("forbidden:") && e.contains("not approved"),
            "{e}"
        );

        assert!(
            f.conn
                .handle(Request {
                    id: 2,
                    kind: Some(run_req("t.spawn", vec![]))
                })
                .await
        );
        wait_until(|| !node.approvals().list().is_empty()).await;
        let pending = f.node.approvals().list().remove(0);
        f.node.approvals().decide(pending.id, true).unwrap();
        assert!(matches!(
            outcome(next_response(&mut f.rx).await.kind.unwrap()),
            Outcome::Spawned(_)
        ));
    }

    #[tokio::test]
    async fn closing_the_connection_withdraws_an_actions_pending_spawn() {
        let mut f = fixture(Quotas {
            spawn_approval: true,
            spawn_approval_timeout_secs: 60,
            ..Quotas::default()
        });
        let a = f.node.spawn(sleeper(&[])).unwrap().id;
        as_agent(&mut f, a);
        assert!(
            f.conn
                .handle(Request {
                    id: 1,
                    kind: Some(run_req("t.spawn", vec![]))
                })
                .await
        );
        let node = Arc::clone(&f.node);
        wait_until(|| !node.approvals().list().is_empty()).await;
        f.conn.actions = ConnActions::new();
        wait_until(|| node.approvals().list().is_empty()).await;
        assert_eq!(f.node.list().unwrap().len(), 1, "nothing was spawned");
    }

    #[tokio::test]
    async fn a_connection_runs_four_actions_at_once_and_queues_the_rest() {
        let mut f = fixture(Quotas::default());
        for id in 0..6 {
            assert!(
                f.conn
                    .handle(Request {
                        id,
                        kind: Some(run_req("t.block", vec![]))
                    })
                    .await
            );
        }
        let flow = Arc::clone(&f.flow);
        wait_until(|| flow.running.load(Ordering::SeqCst) == MAX_PER_CONNECTION).await;
        // The connection still answers other requests meanwhile.
        names(call(&mut f, 50, request::Kind::ListActions(ListActions {})).await);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.flow.running.load(Ordering::SeqCst), MAX_PER_CONNECTION);
        f.flow.gate.add_permits(6);
        for _ in 0..6 {
            let r = next_response(&mut f.rx).await;
            assert_eq!(outcome(r.kind.unwrap()), done("unblocked".into()));
        }
        assert_eq!(
            f.flow.most_running.load(Ordering::SeqCst),
            MAX_PER_CONNECTION
        );
    }

    #[tokio::test]
    async fn the_daemon_runs_sixteen_actions_at_once() {
        let mut f = fixture(Quotas::default());
        // Other connections' actions, sharing the daemon's limit.
        let mut others = Vec::new();
        for _ in 0..5 {
            let (_server, mut conn, rx) = test_server_conn(Arc::clone(&f.node));
            conn.shared = Arc::clone(&f.conn.shared);
            for id in 0..4 {
                assert!(
                    conn.handle(Request {
                        id,
                        kind: Some(run_req("t.block", vec![]))
                    })
                    .await
                );
            }
            others.push((conn, rx));
        }
        let flow = Arc::clone(&f.flow);
        wait_until(|| flow.running.load(Ordering::SeqCst) == MAX_PER_DAEMON).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.flow.running.load(Ordering::SeqCst), MAX_PER_DAEMON);
        f.flow.gate.add_permits(20);
        for (_, rx) in &mut others {
            for _ in 0..4 {
                next_response(rx).await;
            }
        }
        assert_eq!(f.flow.most_running.load(Ordering::SeqCst), MAX_PER_DAEMON);
        drop(f.rx.try_recv());
    }

    #[tokio::test]
    async fn a_closed_connection_cancels_its_action_at_the_next_host_call() {
        let mut f = fixture(Quotas::default());
        assert!(
            f.conn
                .handle(Request {
                    id: 1,
                    kind: Some(run_req("t.block", vec![]))
                })
                .await
        );
        let flow = Arc::clone(&f.flow);
        wait_until(|| flow.running.load(Ordering::SeqCst) == 1).await;
        f.conn.actions = ConnActions::new();
        f.flow.gate.add_permits(1);
        wait_until(|| !flow.after_gate.lock().unwrap().is_empty()).await;
        assert_eq!(f.flow.after_gate.lock().unwrap()[0], "Err(Cancelled)");
    }

    #[tokio::test]
    async fn storage_is_the_workflows_own_in_the_node() {
        let mut f = fixture(Quotas::default());
        assert_eq!(
            outcome(call(&mut f, 1, run_req("t.store", vec![])).await),
            done("v".into())
        );
        assert_eq!(f.node.kv_get("t", "k").unwrap().as_deref(), Some(&b"v"[..]));
    }

    fn argv(items: &[&str]) -> Value {
        Value::List(fleet_proto::StringList {
            items: items.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    #[tokio::test]
    async fn exec_runs_on_the_node_with_the_workflows_env() {
        let mut f = fixture(Quotas::default());
        let got = outcome(
            call(
                &mut f,
                1,
                run_req(
                    "t.exec",
                    vec![(
                        "argv",
                        argv(&["sh", "-c", "echo $T_VAR; echo err >&2; exit 3"]),
                    )],
                ),
            )
            .await,
        );
        assert_eq!(got, done(r#"Some(3) "set\n" "err\n" false"#.into()));
    }

    #[tokio::test]
    async fn exec_is_held_to_the_workflows_capabilities() {
        let mut f = fixture(Quotas::default());
        let e = error_text(call(&mut f, 1, run_req("t.exec", vec![("argv", argv(&["ls"]))])).await);
        assert!(e.starts_with("forbidden:") && e.contains("exec:ls"), "{e}");
    }

    #[tokio::test]
    async fn read_file_and_list_dir_read_the_node_under_the_workflows_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("f.txt"), b"hello").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let mut f = fixture(Quotas::default());
        f.flow = Arc::new(TestFlow {
            read_root: root.to_string_lossy().into_owned(),
            ..TestFlow::default()
        });
        let (server, conn, rx) = test_server_conn(Arc::clone(&f.node));
        server.shared.actions.preset(vec![f.flow.clone()]);
        (f._server, f.conn, f.rx) = (server, conn, rx);
        let file = root.join("f.txt").to_string_lossy().into_owned();
        let got = outcome(
            call(
                &mut f,
                1,
                run_req("t.read", vec![("path", Value::String(file))]),
            )
            .await,
        );
        assert_eq!(got, done("ell 5 false".into()));
        let d = root.to_string_lossy().into_owned();
        let got = outcome(call(&mut f, 2, run_req("t.ls", vec![("path", Value::String(d))])).await);
        assert_eq!(got, done("f.txt:File sub:Dir".into()));
        let e = error_text(
            call(
                &mut f,
                3,
                run_req("t.read", vec![("path", Value::String("/etc/hosts".into()))]),
            )
            .await,
        );
        assert!(e.starts_with("forbidden:"), "{e}");
        let missing = root.join("nope").to_string_lossy().into_owned();
        let e = error_text(
            call(
                &mut f,
                4,
                run_req("t.read", vec![("path", Value::String(missing))]),
            )
            .await,
        );
        assert!(!e.starts_with("forbidden:") && !e.is_empty(), "{e}");
    }

    #[tokio::test]
    async fn actions_are_refused_while_fleetd_restarts() {
        let mut f = fixture(Quotas::default());
        let restarts = super::super::Restarts(Arc::clone(&f.conn.shared));
        let stopped = restarts.stop_requests(std::time::Instant::now() + Duration::from_secs(5));
        assert!(stopped.is_some());
        let e = error_text(call(&mut f, 1, run_req("t.echo", vec![])).await);
        assert_eq!(e, "fleetd is restarting");
    }
}
