//! A fake [`Host`] for tests: records calls, answers from canned data,
//! keeps storage in memory.

use std::collections::BTreeMap;
use std::sync::Mutex;

use fleet_proto::{AgentInfo, AgentState, Forgotten, SpawnAgent};

use crate::host::*;

pub struct FakeHost {
    pub caller: Caller,
    pub agents: Vec<AgentInfo>,
    pub calls: Mutex<Vec<String>>,
    pub store: FakeStorage,
}

#[derive(Default)]
pub struct FakeStorage(pub Mutex<BTreeMap<String, Vec<u8>>>);

pub fn agent(id: u64, name: &str) -> AgentInfo {
    AgentInfo {
        id,
        state: AgentState::Running as i32,
        labels: [("name".to_string(), name.to_string())].into(),
        ..Default::default()
    }
}

impl FakeHost {
    pub fn new(caller: Caller) -> Self {
        FakeHost {
            caller,
            agents: Vec::new(),
            calls: Mutex::new(Vec::new()),
            store: FakeStorage::default(),
        }
    }
    pub fn with_agents(mut self, agents: Vec<AgentInfo>) -> Self {
        self.agents = agents;
        self
    }
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

fn ready<'a, T: Send + 'a>(v: T) -> BoxFuture<'a, T> {
    Box::pin(std::future::ready(v))
}

impl Host for FakeHost {
    fn caller(&self) -> Caller {
        self.caller
    }
    fn exec(&self, req: ExecRequest) -> BoxFuture<'_, HostResult<ExecOutput>> {
        self.record(format!("exec {}", req.argv.join(" ")));
        ready(Ok(ExecOutput {
            exit_code: Some(0),
            stdout: b"out".to_vec(),
            ..Default::default()
        }))
    }
    fn read_file(&self, req: ReadFileRequest) -> BoxFuture<'_, HostResult<FileData>> {
        self.record(format!("read {}", req.path));
        ready(Ok(FileData::default()))
    }
    fn list_dir(&self, req: ListDirRequest) -> BoxFuture<'_, HostResult<DirListing>> {
        self.record(format!("ls {}", req.path));
        ready(Ok(DirListing::default()))
    }
    fn spawn(&self, req: SpawnAgent) -> BoxFuture<'_, HostResult<AgentInfo>> {
        self.record(format!("spawn {}", req.program));
        ready(Ok(AgentInfo {
            id: 99,
            program: req.program,
            labels: req.labels,
            ..Default::default()
        }))
    }
    fn check_spawn(&self) -> BoxFuture<'_, HostResult<()>> {
        self.record("check_spawn".into());
        ready(Ok(()))
    }
    fn list_agents(&self, selector: Labels) -> BoxFuture<'_, HostResult<Vec<AgentInfo>>> {
        self.record("list".into());
        let agents = self
            .agents
            .iter()
            .filter(|a| selector.iter().all(|(k, v)| a.labels.get(k) == Some(v)))
            .cloned()
            .collect();
        ready(Ok(agents))
    }
    fn set_labels(
        &self,
        agent: u64,
        set: Labels,
        remove: Vec<String>,
    ) -> BoxFuture<'_, HostResult<()>> {
        let mut keys: Vec<_> = set.keys().cloned().chain(remove).collect();
        keys.sort();
        self.record(format!("labels {agent} {}", keys.join(",")));
        ready(Ok(()))
    }
    fn forget(&self, ids: Vec<u64>) -> BoxFuture<'_, HostResult<Forgotten>> {
        self.record(format!("forget {ids:?}"));
        ready(Ok(Forgotten {
            ids,
            refused: Vec::new(),
        }))
    }
    fn storage(&self) -> &dyn Storage {
        &self.store
    }
}

impl Storage for FakeStorage {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, HostResult<Option<Vec<u8>>>> {
        ready(Ok(self.0.lock().unwrap().get(key).cloned()))
    }
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, HostResult<()>> {
        self.0.lock().unwrap().insert(key.into(), value);
        ready(Ok(()))
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, HostResult<bool>> {
        ready(Ok(self.0.lock().unwrap().remove(key).is_some()))
    }
    fn keys<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, HostResult<Vec<String>>> {
        let keys = self
            .0
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        ready(Ok(keys))
    }
}
