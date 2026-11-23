//! A [`Host`] for the workflow's integration tests: runs real programs
//! (git on temp repositories, never the network), reads real files, keeps
//! agents and storage in memory.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use fleet_proto::{AgentInfo, AgentState, Forgotten, SpawnAgent};

use crate::error::ActionError;
use crate::fake::FakeStorage;
use crate::host::*;

pub struct LocalHost {
    pub caller: Caller,
    pub agents: Mutex<Vec<AgentInfo>>,
    pub store: FakeStorage,
    next_id: AtomicU64,
    /// `spawn` fails with this (an approval denied, a quota raced).
    pub refuse_spawn: Mutex<Option<ActionError>>,
    /// `check_spawn` fails with this.
    pub refuse_check: Mutex<Option<ActionError>>,
    pub execs: Mutex<Vec<Vec<String>>>,
}

impl LocalHost {
    pub fn new(caller: Caller) -> Self {
        LocalHost {
            caller,
            agents: Mutex::new(Vec::new()),
            store: FakeStorage::default(),
            next_id: AtomicU64::new(1),
            refuse_spawn: Mutex::new(None),
            refuse_check: Mutex::new(None),
            execs: Mutex::new(Vec::new()),
        }
    }

    pub fn agent(&self, id: u64) -> AgentInfo {
        let agents = self.agents.lock().unwrap();
        agents.iter().find(|a| a.id == id).cloned().expect("agent")
    }

    /// Adds an agent (as if spawned elsewhere) running in `cwd`.
    pub fn add_agent(&self, cwd: &str, labels: &[(&str, &str)], running: bool) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let state = if running {
            AgentState::Running
        } else {
            AgentState::Exited
        };
        self.agents.lock().unwrap().push(AgentInfo {
            id,
            cwd: cwd.into(),
            state: state as i32,
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            ..Default::default()
        });
        id
    }

    pub fn set_state(&self, id: u64, state: AgentState) {
        let mut agents = self.agents.lock().unwrap();
        if let Some(a) = agents.iter_mut().find(|a| a.id == id) {
            a.state = state as i32;
        }
    }
}

fn ready<'a, T: Send + 'a>(v: T) -> BoxFuture<'a, T> {
    Box::pin(std::future::ready(v))
}

fn io(e: std::io::Error) -> ActionError {
    ActionError::failed(e.to_string())
}

impl Host for LocalHost {
    fn caller(&self) -> Caller {
        self.caller
    }

    fn exec(&self, req: ExecRequest) -> BoxFuture<'_, HostResult<ExecOutput>> {
        self.execs.lock().unwrap().push(req.argv.clone());
        let mut cmd = Command::new(&req.argv[0]);
        cmd.args(&req.argv[1..])
            .current_dir(&req.cwd)
            .stdin(Stdio::null())
            // Tests never reach a user's git config or a network.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        let r = cmd.output().map_err(io).map(|o| ExecOutput {
            exit_code: o.status.code(),
            stdout: o.stdout,
            stderr: o.stderr,
            ..Default::default()
        });
        ready(r)
    }

    fn read_file(&self, req: ReadFileRequest) -> BoxFuture<'_, HostResult<FileData>> {
        let r = (|| {
            let all = std::fs::read(&req.path)?;
            let start = (req.offset as usize).min(all.len());
            let max = if req.max_bytes == 0 {
                usize::MAX
            } else {
                req.max_bytes as usize
            };
            let end = start.saturating_add(max).min(all.len());
            Ok(FileData {
                data: all[start..end].to_vec(),
                size: all.len() as u64,
                mtime_unix_ms: 0,
                eof: end == all.len(),
            })
        })()
        .map_err(io);
        ready(r)
    }

    fn list_dir(&self, req: ListDirRequest) -> BoxFuture<'_, HostResult<DirListing>> {
        let r = std::fs::read_dir(&req.path).map_err(io).map(|rd| DirListing {
            entries: rd
                .filter_map(Result::ok)
                .map(|e| DirEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    ..Default::default()
                })
                .collect(),
            truncated: false,
        });
        ready(r)
    }

    fn spawn(&self, req: SpawnAgent) -> BoxFuture<'_, HostResult<AgentInfo>> {
        if let Some(e) = self.refuse_spawn.lock().unwrap().clone() {
            return ready(Err(e));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let info = AgentInfo {
            id,
            program: req.program,
            args: req.args,
            cwd: req.cwd,
            state: AgentState::Running as i32,
            labels: req.labels,
            ..Default::default()
        };
        self.agents.lock().unwrap().push(info.clone());
        ready(Ok(info))
    }

    fn check_spawn(&self) -> BoxFuture<'_, HostResult<()>> {
        match self.refuse_check.lock().unwrap().clone() {
            Some(e) => ready(Err(e)),
            None => ready(Ok(())),
        }
    }

    fn list_agents(&self, selector: Labels) -> BoxFuture<'_, HostResult<Vec<AgentInfo>>> {
        let agents = self.agents.lock().unwrap();
        let got = agents
            .iter()
            .filter(|a| selector.iter().all(|(k, v)| a.labels.get(k) == Some(v)))
            .cloned()
            .collect();
        ready(Ok(got))
    }

    fn set_labels(
        &self,
        agent: u64,
        set: Labels,
        remove: Vec<String>,
    ) -> BoxFuture<'_, HostResult<()>> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(a) = agents.iter_mut().find(|a| a.id == agent) {
            a.labels.extend(set);
            for k in remove {
                a.labels.remove(&k);
            }
        }
        ready(Ok(()))
    }

    fn forget(&self, ids: Vec<u64>) -> BoxFuture<'_, HostResult<Forgotten>> {
        let mut agents = self.agents.lock().unwrap();
        agents.retain(|a| !ids.contains(&a.id));
        ready(Ok(Forgotten {
            ids,
            refused: Vec::new(),
        }))
    }

    fn storage(&self) -> &dyn Storage {
        &self.store
    }
}

/// A temp folder with a repository `repo` (one commit on `main`) and a bare
/// `origin` it was pushed to, so `origin/main` and `origin/HEAD` exist with
/// no network.
pub struct Repos {
    pub dir: tempfile::TempDir,
    /// The temp folder, its symlinks resolved (macOS's `/var` → `/private/var`).
    pub root: String,
    pub main: String,
}

pub fn git(cwd: &str, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

impl Repos {
    pub fn new() -> Repos {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root = root.to_string_lossy().into_owned();
        let main = format!("{root}/repo");
        git(&root, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        git(&root, &["init", "-q", "-b", "main", "repo"]);
        std::fs::write(format!("{main}/a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(format!("{main}/.gitignore"), "target/\n").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "init"]);
        git(&main, &["remote", "add", "origin", &format!("{root}/origin.git")]);
        git(&main, &["push", "-q", "origin", "main"]);
        git(&main, &["remote", "set-head", "origin", "main"]);
        Repos { dir, root, main }
    }

    pub fn write(&self, path: &str, text: &str) {
        let p = std::path::Path::new(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// The config text putting worktrees under `<root>/wt/{repo}`, with
    /// `fetch_base` as given.
    pub fn config(&self, fetch_base: bool) -> String {
        format!(
            "[workflows.git-worktree]\nroot = \"{}/wt/{{repo}}\"\nfetch_base = {fetch_base}\n",
            self.root
        )
    }
}

/// Labels as a map.
pub fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}
