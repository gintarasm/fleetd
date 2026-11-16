//! The node's side of mailboxes (spec §3, ADR 0076): sending, reading,
//! automatic reports and the checks behind `fleet wait`.
//!
//! Nothing here runs on the PTY output path. An exit report is written by
//! the exiting agent's sink after its stream has ended, like its exit code.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Mutex;

use fleet_proto::{AgentInfo, Broadcast, MessagesSent, SendMessage};
use tokio::sync::broadcast;
use tracing::warn;

use super::{Node, invalid, labels_match, lock, store_err};
use crate::mailbox::{
    MAX_BODY, Mailbox, Message, MessageKind, NewMessage, Notice, Status, WaitMatch, WaitState,
};
use crate::scope::{self, Caller};
use crate::store::{AgentState, Store};

/// Most messages one inbox read returns.
pub const INBOX_PAGE_MAX: usize = 1000;

/// Notices queued per waiter before it lags (and re-checks its mailbox).
pub(super) const NOTICE_CAPACITY: usize = 256;

fn not_found(id: u64) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("agent {id} not found"))
}

/// The caller's own mailbox.
pub fn mailbox_of(caller: Caller) -> Mailbox {
    match caller {
        Caller::User => None,
        Caller::Agent(a) => Some(a),
    }
}

fn sender_of(caller: Caller) -> Option<u64> {
    mailbox_of(caller)
}

/// What `fleet wait` waits for (see [`Node::wait_check`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitSpec {
    /// Named agents; `None`: the caller's descendants (every agent for the
    /// user), news only.
    pub targets: Option<Vec<u64>>,
    pub states: HashSet<WaitState>,
}

/// Posts an exiting agent's `exit` report and announces it. Runs on the
/// agent's reader thread once its output has ended.
pub(super) fn report_exit(
    store: &Mutex<Store>,
    notices: &broadcast::Sender<Notice>,
    agent: u64,
    code: i32,
) {
    let posted = lock(store).post_report(agent, |to| NewMessage {
        recipient: to,
        sender: Some(agent),
        kind: MessageKind::Exit,
        body: format!("exited with code {code}"),
        status: None,
        exit_code: Some(code),
    });
    match posted {
        Ok(Some((recipient, _))) => {
            let _ = notices.send(Notice::Message { recipient });
        }
        Ok(None) => {}
        Err(e) => warn!(agent, error = %e, "posting the exit report failed"),
    }
    // No receivers is fine.
    let _ = notices.send(Notice::State {
        agent,
        state: WaitState::Exited,
    });
}

/// The wire form of `m`; `sender_name` is the sender's name now.
pub fn message_proto(m: &Message, sender_name: String) -> fleet_proto::Message {
    use fleet_proto::MessageKind as K;
    let kind = match m.kind {
        MessageKind::Message => K::Message,
        MessageKind::Status => K::Status,
        MessageKind::Exit => K::Exit,
        MessageKind::Lost => K::Lost,
        MessageKind::Question => K::Question,
        MessageKind::Answer => K::Answer,
    };
    fleet_proto::Message {
        id: m.id,
        sender: m.sender,
        recipient: m.recipient,
        kind: kind as i32,
        body: m.body.clone(),
        created_unix_ms: m.created_unix_ms,
        sender_name,
        status: m.status.clone().unwrap_or_default(),
        exit_code: m.exit_code,
        choices: m.choices.clone(),
    }
}

impl Node {
    /// Receives the node's mailbox notices from now on (for waiters).
    pub fn notices(&self) -> broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }

    /// Delivers a message from `caller` (ADR 0034 §3, §5). An agent may
    /// write to its descendants, its parent and the user; the user to
    /// anyone. A broadcast goes to running agents only, never to a root
    /// shell (that is the user's own terminal).
    pub fn send_message(&self, caller: Caller, req: &SendMessage) -> io::Result<MessagesSent> {
        if req.body.is_empty() {
            return Err(invalid("a message must not be empty".into()));
        }
        if req.body.len() > MAX_BODY {
            return Err(invalid(format!(
                "a message is at most {MAX_BODY} bytes (this one is {})",
                req.body.len()
            )));
        }
        let broadcast = Broadcast::try_from(req.broadcast)
            .map_err(|_| invalid(format!("unknown broadcast {}", req.broadcast)))?;
        if broadcast == Broadcast::None && !req.selector.is_empty() {
            return Err(invalid("a label selector needs a broadcast".into()));
        }
        if req.agent_ids.is_empty()
            && !req.to_user
            && !req.to_parent
            && broadcast == Broadcast::None
        {
            return Err(invalid("a message needs a recipient".into()));
        }
        let tree = self.tree()?;
        let mut agents: Vec<u64> = Vec::new();
        let mut user = req.to_user;
        {
            let db = lock(&self.store);
            for &id in &req.agent_ids {
                let allowed = match caller {
                    Caller::User => true,
                    Caller::Agent(a) => {
                        tree.is_descendant(a, id)
                            || db.report_mailbox(a).map_err(store_err)? == Some(Some(id))
                    }
                };
                if !allowed {
                    let Caller::Agent(a) = caller else {
                        unreachable!()
                    };
                    return Err(scope::forbidden(format!(
                        "agent {id} is not a descendant or the parent of agent {a}"
                    )));
                }
                let Some(labels) = db.labels(id).map_err(store_err)? else {
                    return Err(not_found(id));
                };
                if scope::is_root_shell(&labels) {
                    return Err(invalid(format!(
                        "agent {id} is a terminal of the user's, not an agent"
                    )));
                }
                agents.push(id);
            }
            if req.to_parent {
                let Caller::Agent(a) = caller else {
                    return Err(invalid("the user has no parent".into()));
                };
                match db.report_mailbox(a).map_err(store_err)? {
                    Some(Some(p)) => agents.push(p),
                    _ => user = true,
                }
            }
        }
        if broadcast != Broadcast::None {
            let wanted = self.broadcast_targets(caller, broadcast, &req.selector)?;
            if wanted.is_empty() && agents.is_empty() && !user {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no running agent matched",
                ));
            }
            agents.extend(wanted);
        }
        let mut seen = HashSet::new();
        agents.retain(|id| seen.insert(*id));
        let message = |recipient| NewMessage {
            recipient,
            sender: sender_of(caller),
            kind: MessageKind::Message,
            body: req.body.clone(),
            status: None,
            exit_code: None,
        };
        let mut all: Vec<NewMessage> = agents.iter().map(|&id| message(Some(id))).collect();
        if user {
            all.push(message(None));
        }
        lock(&self.store).post_messages(&all).map_err(store_err)?;
        for m in &all {
            let _ = self.notices.send(Notice::Message {
                recipient: m.recipient,
            });
        }
        Ok(MessagesSent {
            agent_ids: agents,
            user,
        })
    }

    /// The running agents a broadcast from `caller` reaches.
    fn broadcast_targets(
        &self,
        caller: Caller,
        broadcast: Broadcast,
        selector: &HashMap<String, String>,
    ) -> io::Result<Vec<u64>> {
        let tree = self.tree()?;
        let mut ids: Vec<u64> = self
            .list()?
            .into_iter()
            .filter(|info| info.state() == fleet_proto::AgentState::Running)
            .filter(|info| !scope::is_root_shell(&info.labels))
            .filter(|info| labels_match(&info.labels, selector))
            .filter(|info| match (caller, broadcast) {
                (Caller::User, Broadcast::Children) => tree.parent(info.id).is_none(),
                (Caller::User, _) => true,
                (Caller::Agent(a), Broadcast::Children) => tree.parent(info.id) == Some(a),
                (Caller::Agent(a), _) => tree.is_descendant(a, info.id),
            })
            .map(|info| info.id)
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// The caller's mailbox after cursor `since` (`None`: after its read
    /// mark), at most `max` messages (capped at [`INBOX_PAGE_MAX`]), each
    /// with its sender's name. What it returns is marked read. The cursor:
    /// the newest id returned, else the one asked from.
    pub fn inbox(
        &self,
        caller: Caller,
        since: Option<u64>,
        max: usize,
    ) -> io::Result<(Vec<fleet_proto::Message>, u64)> {
        let mailbox = mailbox_of(caller);
        let max = if max == 0 {
            INBOX_PAGE_MAX
        } else {
            max.min(INBOX_PAGE_MAX)
        };
        let db = lock(&self.store);
        let after = match since {
            Some(c) => c,
            None => db.read_mark(mailbox).map_err(store_err)?,
        };
        let messages = db.messages_after(mailbox, after, max).map_err(store_err)?;
        let cursor = messages.last().map_or(after, |m| m.id);
        db.mark_read(mailbox, cursor).map_err(store_err)?;
        let mut names = HashMap::new();
        let out = messages
            .iter()
            .map(|m| {
                let name = match m.sender {
                    None => String::new(),
                    Some(s) => names.entry(s).or_insert_with(|| name_of(&db, s)).clone(),
                };
                message_proto(m, name)
            })
            .collect();
        Ok((out, cursor))
    }

    /// `agent`'s reported (or inferred) status changed: posts a `status`
    /// message to its parent (or the user) and wakes waiters. A report that
    /// repeats the last one (state and text) posts nothing, so a parent gets
    /// one message per change. `true` if it posted.
    ///
    /// The seam for M3-D: `fleet report` calls this after storing the
    /// report, and so does the shared inferred-`waiting` heuristic on its
    /// transitions (with `inferred`).
    pub fn status_changed(
        &self,
        agent: u64,
        status: Status,
        message: &str,
        inferred: bool,
    ) -> io::Result<bool> {
        let body = match (inferred, message.is_empty()) {
            (true, true) => format!("{} (inferred)", status.as_str()),
            (true, false) => format!("{message} (inferred)"),
            (false, _) => message.to_owned(),
        };
        let posted = {
            let db = lock(&self.store);
            let last = db.latest_status_message(agent).map_err(store_err)?;
            if last
                .as_ref()
                .is_some_and(|m| m.status.as_deref() == Some(status.as_str()) && m.body == body)
            {
                return Ok(false);
            }
            db.post_report(agent, |to| NewMessage {
                recipient: to,
                sender: Some(agent),
                kind: MessageKind::Status,
                body,
                status: Some(status.as_str().into()),
                exit_code: None,
            })
            .map_err(store_err)?
        };
        let Some((recipient, _)) = posted else {
            return Ok(false);
        };
        let _ = self.notices.send(Notice::Message { recipient });
        if let Some(state) = WaitState::of_status(status) {
            let _ = self.notices.send(Notice::State { agent, state });
        }
        Ok(true)
    }

    /// Whether `agent` is inferred to be waiting (spec §2: no output for
    /// 30 s at a prompt-like last line).
    ///
    /// The seam for M3-D's shared heuristic: until it lands nothing is
    /// inferred, and `fleet wait --state waiting` matches reported
    /// `waiting` only.
    fn inferred_waiting(&self, _agent: u64) -> bool {
        false
    }

    /// What a wait by `caller` matches now: unread reports and messages in
    /// its mailbox (from a target, when targets are named), the current
    /// state of each named target, and `notice` (a transition of a target
    /// that just happened). Empty: keep waiting. A named target that does
    /// not exist (any more) is an error.
    pub fn wait_check(
        &self,
        caller: Caller,
        spec: &WaitSpec,
        notice: Option<&Notice>,
    ) -> io::Result<Vec<WaitMatch>> {
        let mut found: Vec<WaitMatch> = Vec::new();
        let mailbox = mailbox_of(caller);
        let wanted = |s: WaitState| spec.states.contains(&s);
        {
            let db = lock(&self.store);
            let mark = db.read_mark(mailbox).map_err(store_err)?;
            for m in db
                .messages_after(mailbox, mark, INBOX_PAGE_MAX)
                .map_err(store_err)?
            {
                let Some(state) = WaitState::of_message(&m) else {
                    continue;
                };
                let from_target = match &spec.targets {
                    None => true,
                    Some(ids) => m.sender.is_some_and(|s| ids.contains(&s)),
                };
                if wanted(state) && from_target {
                    found.push(WaitMatch {
                        agent: m.sender,
                        state,
                        message: Some(m),
                    });
                }
            }
            for &id in spec.targets.iter().flatten() {
                let Some(rec) = db.agent(id).map_err(store_err)? else {
                    return Err(not_found(id));
                };
                let mut now = Vec::new();
                match rec.state {
                    AgentState::Exited => now.push(WaitState::Exited),
                    AgentState::Lost => now.push(WaitState::Lost),
                    AgentState::Running => {}
                }
                // The agent's stored report (ADR 0075).
                if let Some(state) = rec
                    .reported
                    .as_ref()
                    .and_then(|r| fleet_proto::ReportedState::try_from(r.state).ok())
                    .and_then(Status::of_reported)
                    .and_then(WaitState::of_status)
                {
                    now.push(state);
                }
                if rec.state == AgentState::Running && self.inferred_waiting(id) {
                    now.push(WaitState::Waiting);
                }
                for state in now.into_iter().filter(|s| wanted(*s)) {
                    found.push(WaitMatch {
                        agent: Some(id),
                        state,
                        message: None,
                    });
                }
            }
        }
        if let Some(Notice::State { agent, state }) = notice
            && wanted(*state)
            && self.is_target(caller, spec, *agent)?
        {
            found.push(WaitMatch {
                agent: Some(*agent),
                state: *state,
                message: None,
            });
        }
        // One match per agent and state, the one with its message first.
        let mut seen = HashSet::new();
        found.sort_by_key(|m| m.message.is_none());
        found.retain(|m| seen.insert((m.agent, m.state)));
        found.sort_by_key(|m| (m.message.as_ref().map(|m| m.id), m.agent));
        Ok(found)
    }

    /// Whether a wait of `caller`'s with `spec` covers `agent`.
    fn is_target(&self, caller: Caller, spec: &WaitSpec, agent: u64) -> io::Result<bool> {
        Ok(match (&spec.targets, caller) {
            (Some(ids), _) => ids.contains(&agent),
            (None, Caller::User) => true,
            (None, Caller::Agent(a)) => self.tree()?.is_descendant(a, agent),
        })
    }

    /// The agents a wait covers, as they are now (what a timeout prints).
    pub fn wait_targets(&self, caller: Caller, spec: &WaitSpec) -> io::Result<Vec<AgentInfo>> {
        let mut agents = self.list_for(caller, &HashMap::new())?;
        if let Some(ids) = &spec.targets {
            agents.retain(|a| ids.contains(&a.id));
        }
        agents.sort_by_key(|a| a.id);
        Ok(agents)
    }
}

/// `agent`'s `name` label (empty: none, or it is gone).
fn name_of(db: &Store, agent: u64) -> String {
    db.labels(agent)
        .ok()
        .flatten()
        .and_then(|mut l| l.remove(scope::NAME_LABEL))
        .unwrap_or_default()
}

/// The wire form of a wait state.
pub fn wait_state_proto(s: WaitState) -> fleet_proto::WaitState {
    use fleet_proto::WaitState as W;
    match s {
        WaitState::Waiting => W::Waiting,
        WaitState::Done => W::Done,
        WaitState::Error => W::Error,
        WaitState::Exited => W::Exited,
        WaitState::Lost => W::Lost,
        WaitState::Message => W::Message,
    }
}

/// A wire wait state; `None` for one this node does not know.
pub fn wait_state_from_proto(s: i32) -> Option<WaitState> {
    use fleet_proto::WaitState as W;
    Some(match W::try_from(s).ok()? {
        W::Unspecified => return None,
        W::Waiting => WaitState::Waiting,
        W::Done => WaitState::Done,
        W::Error => WaitState::Error,
        W::Exited => WaitState::Exited,
        W::Lost => WaitState::Lost,
        W::Message => WaitState::Message,
    })
}

impl Node {
    /// The answer to a wait: `matches` (empty on a timeout), with each
    /// message's sender name, and the targets as they are now.
    pub fn wait_result(
        &self,
        caller: Caller,
        spec: &WaitSpec,
        matches: Vec<WaitMatch>,
    ) -> io::Result<fleet_proto::WaitResult> {
        let matches = {
            let db = lock(&self.store);
            matches
                .into_iter()
                .map(|m| fleet_proto::WaitMatch {
                    agent_id: m.agent,
                    state: wait_state_proto(m.state) as i32,
                    message: m.message.map(|msg| {
                        let name = msg.sender.map(|s| name_of(&db, s)).unwrap_or_default();
                        message_proto(&msg, name)
                    }),
                })
                .collect()
        };
        Ok(fleet_proto::WaitResult {
            matches,
            agents: self.wait_targets(caller, spec)?,
        })
    }
}
