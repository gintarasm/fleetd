//! The [`Workflow`] trait and the [`Registry`] that lists and runs their
//! actions (ADR 0092).

use std::collections::HashSet;
use std::sync::Arc;

use fleet_proto::action_result::Outcome;
use fleet_proto::{ActionCallers, ActionSpec, ParamKind, RunAction};

use crate::checked::{CancelFlag, Checked};
use crate::error::ActionError;
use crate::host::{BoxFuture, Caller, Host};
use crate::manifest::Manifest;
use crate::params::{Params, validate};

/// What an action returns when it does not fail: `done`, `spawned`, a
/// `view` or an overridable `refused`.
pub type ActionOutcome = Outcome;

/// One call of an action, as the workflow gets it: already validated.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionCall {
    /// The action's name without the workflow's (`new-agent` for
    /// `git-worktree.new-agent`).
    pub action: String,
    pub params: Params,
    /// The caller's working directory (the CLI sends the shell's).
    pub cwd: String,
}

/// A workflow: native now, a WASM component behind the same trait in M5.
pub trait Workflow: Send + Sync {
    /// `[a-z0-9-]+`, the prefix of its actions' names (`git-worktree`).
    fn id(&self) -> &str;
    /// What it uses; the host holds it to this.
    fn manifest(&self) -> Result<Manifest, String>;
    /// Its actions; static (the registry reads them once).
    fn actions(&self) -> Vec<ActionSpec>;
    /// Runs one validated call. `host` acts as the caller and is checked
    /// against the manifest.
    fn run<'a>(
        &'a self,
        host: &'a dyn Host,
        call: ActionCall,
    ) -> BoxFuture<'a, Result<ActionOutcome, ActionError>>;
}

/// What the daemon needs to build a workflow's host for one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowInfo {
    /// Its storage's namespace.
    pub id: String,
    /// The label prefix it owns (its manifest's `labels:`), for spawns.
    pub owned_prefix: Option<String>,
}

struct Entry {
    workflow: Arc<dyn Workflow>,
    manifest: Manifest,
    info: WorkflowInfo,
}

/// The daemon's workflows and their actions, checked once when built.
pub struct Registry {
    entries: Vec<Entry>,
    /// Every action, in the workflows' order, with its entry's index.
    actions: Vec<(ActionSpec, usize)>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn check_spec(spec: &ActionSpec) -> Result<(), String> {
    let mut keys = HashSet::new();
    for p in &spec.params {
        if p.key.is_empty() || !keys.insert(p.key.as_str()) {
            return Err(format!(
                "{}: empty or repeated param {:?}",
                spec.name, p.key
            ));
        }
        match p.kind() {
            ParamKind::Unspecified => {
                return Err(format!("{}: param {:?} has no kind", spec.name, p.key));
            }
            ParamKind::ParamChoice if p.choices.is_empty() => {
                return Err(format!("{}: choice {:?} has no choices", spec.name, p.key));
            }
            _ => {}
        }
    }
    if !spec.target_param.is_empty()
        && !spec
            .params
            .iter()
            .any(|p| p.key == spec.target_param && p.kind() == ParamKind::ParamAgent)
    {
        return Err(format!(
            "{}: target {:?} is not an agent param",
            spec.name, spec.target_param
        ));
    }
    Ok(())
}

impl Registry {
    /// Checks every workflow (id, manifest, action names and schemas);
    /// `Err` names the first problem.
    pub fn new(workflows: Vec<Arc<dyn Workflow>>) -> Result<Registry, String> {
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for workflow in workflows {
            let id = workflow.id().to_owned();
            if !valid_id(&id) || !ids.insert(id.clone()) {
                return Err(format!("bad or repeated workflow id {id:?}"));
            }
            let manifest = workflow
                .manifest()
                .map_err(|e| format!("workflow {id}: {e}"))?;
            for spec in workflow.actions() {
                let short = spec
                    .name
                    .strip_prefix(&id)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .unwrap_or("");
                if short.is_empty() || !names.insert(spec.name.clone()) {
                    return Err(format!(
                        "workflow {id}: bad or repeated action name {:?}",
                        spec.name
                    ));
                }
                check_spec(&spec)?;
                actions.push((spec, entries.len()));
            }
            let info = WorkflowInfo {
                id,
                owned_prefix: manifest.owned_prefix().map(str::to_owned),
            };
            entries.push(Entry {
                workflow,
                manifest,
                info,
            });
        }
        Ok(Registry { entries, actions })
    }

    /// The actions `caller` may call.
    pub fn list(&self, caller: Caller) -> Vec<ActionSpec> {
        self.actions
            .iter()
            .filter(|(spec, _)| {
                caller == Caller::User || spec.callers() == ActionCallers::UserAndAgents
            })
            .map(|(spec, _)| spec.clone())
            .collect()
    }

    /// Runs `req` on the host `make_host` builds for the action's workflow
    /// (acting as the caller): validated first, then held to the manifest
    /// and to `cancel`.
    pub async fn run<'h, H: Host + 'h>(
        &self,
        req: RunAction,
        cancel: CancelFlag,
        make_host: impl FnOnce(&WorkflowInfo) -> H,
    ) -> Result<ActionOutcome, ActionError> {
        let Some((spec, at)) = self.actions.iter().find(|(s, _)| s.name == req.name) else {
            return Err(ActionError::invalid(format!("no action {:?}", req.name)));
        };
        let entry = &self.entries[*at];
        let raw = make_host(&entry.info);
        let host = Checked::new(&raw, &entry.manifest, cancel);
        let params = validate(spec, &host, entry.info.owned_prefix.as_deref(), req.params).await?;
        let action = spec.name[entry.info.id.len() + 1..].to_owned();
        let call = ActionCall {
            action,
            params,
            cwd: req.cwd,
        };
        entry.workflow.run(&host, call).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeHost, agent};
    use crate::host::{Caller, ExecRequest};
    use fleet_proto::param_value::Value;
    use fleet_proto::{ActionCallers, ActionDone, ParamKind, ParamSpec, ParamValue};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Echoes what it got; `exec` tries to run `sh` (not in its manifest).
    struct Echo {
        id: &'static str,
        caps: &'static [&'static str],
        names: Vec<&'static str>,
        seen: Mutex<Vec<String>>,
    }

    fn echo() -> Echo {
        Echo {
            id: "echo",
            caps: &["exec:git", "labels:echo."],
            names: vec!["echo.say", "echo.exec", "echo.mine"],
            seen: Mutex::new(Vec::new()),
        }
    }

    impl Workflow for Echo {
        fn id(&self) -> &str {
            self.id
        }
        fn manifest(&self) -> Result<Manifest, String> {
            Manifest::parse(self.caps)
        }
        fn actions(&self) -> Vec<ActionSpec> {
            self.names
                .iter()
                .map(|n| ActionSpec {
                    name: n.to_string(),
                    callers: if n.ends_with("mine") {
                        ActionCallers::User
                    } else {
                        ActionCallers::UserAndAgents
                    } as i32,
                    params: vec![
                        ParamSpec {
                            key: "text".into(),
                            kind: ParamKind::ParamString as i32,
                            ..Default::default()
                        },
                        ParamSpec {
                            key: "agent".into(),
                            kind: ParamKind::ParamAgent as i32,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })
                .collect()
        }
        fn run<'a>(
            &'a self,
            host: &'a dyn Host,
            call: ActionCall,
        ) -> BoxFuture<'a, Result<ActionOutcome, ActionError>> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(format!(
                    "{} {:?} {:?} {}",
                    call.action,
                    call.params.string("text"),
                    call.params.agent("agent"),
                    call.cwd
                ));
                if call.action == "exec" {
                    host.exec(ExecRequest {
                        argv: vec!["sh".into()],
                        ..Default::default()
                    })
                    .await?;
                }
                if call.action == "say" && call.params.string("text") == Some("git") {
                    host.exec(ExecRequest {
                        argv: vec!["git".into()],
                        ..Default::default()
                    })
                    .await?;
                }
                Ok(Outcome::Done(ActionDone {
                    message: "ok".into(),
                }))
            })
        }
    }

    fn run_req(name: &str, params: Vec<(&str, Value)>) -> RunAction {
        RunAction {
            name: name.into(),
            params: params
                .into_iter()
                .map(|(k, v)| (k.to_string(), ParamValue { value: Some(v) }))
                .collect::<HashMap<_, _>>(),
            cwd: "/here".into(),
        }
    }

    #[test]
    fn agents_list_only_actions_open_to_them() {
        let r = Registry::new(vec![Arc::new(echo())]).unwrap();
        let names = |c| -> Vec<String> { r.list(c).into_iter().map(|a| a.name).collect() };
        assert_eq!(names(Caller::User), ["echo.say", "echo.exec", "echo.mine"]);
        assert_eq!(names(Caller::Agent(4)), ["echo.say", "echo.exec"]);
    }

    #[test]
    fn a_bad_workflow_is_refused_at_build() {
        let bad = |w: Echo| Registry::new(vec![Arc::new(w)]).is_err();
        let mut w = echo();
        w.names = vec!["other.say"];
        assert!(bad(w), "an action outside the workflow's namespace");
        let mut w = echo();
        w.names = vec!["echo.say", "echo.say"];
        assert!(bad(w), "duplicate actions");
        let mut w = echo();
        w.names = vec!["echo."];
        assert!(bad(w), "an empty action name");
        let mut w = echo();
        w.caps = &["exec:"];
        assert!(bad(w), "a bad manifest");
        let mut w = echo();
        w.id = "Bad Id";
        w.names = vec!["Bad Id.x"];
        assert!(bad(w), "a bad id");
        assert!(
            Registry::new(vec![Arc::new(echo()), Arc::new(echo())]).is_err(),
            "duplicate workflows"
        );
        assert!(
            Registry::new(Vec::new())
                .unwrap()
                .list(Caller::User)
                .is_empty()
        );
    }

    #[test]
    fn a_bad_param_spec_is_refused_at_build() {
        struct BadParams(Vec<ParamSpec>, &'static str);
        impl Workflow for BadParams {
            fn id(&self) -> &str {
                "bad"
            }
            fn manifest(&self) -> Result<Manifest, String> {
                Ok(Manifest::default())
            }
            fn actions(&self) -> Vec<ActionSpec> {
                vec![ActionSpec {
                    name: "bad.x".into(),
                    params: self.0.clone(),
                    target_param: self.1.into(),
                    ..Default::default()
                }]
            }
            fn run<'a>(
                &'a self,
                _: &'a dyn Host,
                _: ActionCall,
            ) -> BoxFuture<'a, Result<ActionOutcome, ActionError>> {
                unreachable!()
            }
        }
        let p = |key: &str, kind: ParamKind| ParamSpec {
            key: key.into(),
            kind: kind as i32,
            ..Default::default()
        };
        let bad = |ps: Vec<ParamSpec>| Registry::new(vec![Arc::new(BadParams(ps, ""))]).is_err();
        let bad_target =
            |ps: Vec<ParamSpec>, t| Registry::new(vec![Arc::new(BadParams(ps, t))]).is_err();
        assert!(
            bad_target(vec![p("a", ParamKind::ParamBool)], "a"),
            "a target is an agent param"
        );
        assert!(bad_target(vec![], "a"), "a target names a param");
        assert!(!bad_target(vec![p("a", ParamKind::ParamAgent)], "a"));
        assert!(bad(vec![p("a", ParamKind::Unspecified)]));
        assert!(bad(vec![
            p("a", ParamKind::ParamBool),
            p("a", ParamKind::ParamInt)
        ]));
        assert!(bad(vec![p("", ParamKind::ParamBool)]));
        assert!(
            bad(vec![p("c", ParamKind::ParamChoice)]),
            "a choice without choices"
        );
        assert!(!bad(vec![p("a", ParamKind::ParamBool)]));
    }

    #[tokio::test]
    async fn run_validates_then_calls_the_workflow_with_a_checked_host() {
        let w = Arc::new(echo());
        let r = Registry::new(vec![w.clone()]).unwrap();
        let fake = FakeHost::new(Caller::User).with_agents(vec![agent(7, "kid")]);
        let mut info = None;
        let out = r
            .run(
                run_req(
                    "echo.say",
                    vec![
                        ("text", Value::String("hi".into())),
                        ("agent", Value::String("kid".into())),
                    ],
                ),
                CancelFlag::default(),
                |wi| {
                    info = Some(wi.clone());
                    &fake
                },
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            Outcome::Done(ActionDone {
                message: "ok".into()
            })
        );
        assert_eq!(
            info,
            Some(WorkflowInfo {
                id: "echo".into(),
                owned_prefix: Some("echo.".into())
            })
        );
        assert_eq!(
            w.seen.lock().unwrap().as_slice(),
            ["say Some(\"hi\") Some(7) /here"]
        );

        // The manifest holds: `sh` is not `exec:git`.
        let e = r
            .run(run_req("echo.exec", vec![]), CancelFlag::default(), |_| {
                &fake
            })
            .await
            .unwrap_err();
        assert!(matches!(e, ActionError::Forbidden(_)), "{e}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("exec")));
    }

    #[tokio::test]
    async fn run_refuses_unknown_actions_and_bad_calls_before_the_workflow() {
        let w = Arc::new(echo());
        let r = Registry::new(vec![w.clone()]).unwrap();
        let user = FakeHost::new(Caller::User);
        let e = r
            .run(run_req("echo.nope", vec![]), CancelFlag::default(), |_| {
                &user
            })
            .await
            .unwrap_err();
        assert!(matches!(e, ActionError::Invalid(_)), "{e}");
        let e = r
            .run(
                run_req("echo.say", vec![("text", Value::Bool(true))]),
                CancelFlag::default(),
                |_| &user,
            )
            .await
            .unwrap_err();
        assert!(matches!(e, ActionError::Invalid(_)), "{e}");
        let agent_host = FakeHost::new(Caller::Agent(3));
        let e = r
            .run(run_req("echo.mine", vec![]), CancelFlag::default(), |_| {
                &agent_host
            })
            .await
            .unwrap_err();
        assert!(matches!(e, ActionError::Forbidden(_)), "{e}");
        assert!(w.seen.lock().unwrap().is_empty(), "the workflow never ran");
    }

    #[tokio::test]
    async fn a_cancelled_call_fails_at_its_next_host_call() {
        let r = Registry::new(vec![Arc::new(echo())]).unwrap();
        let fake = FakeHost::new(Caller::User);
        let cancel = CancelFlag::default();
        cancel.cancel();
        let e = r
            .run(
                run_req("echo.say", vec![("text", Value::String("git".into()))]),
                cancel,
                |_| &fake,
            )
            .await
            .unwrap_err();
        assert_eq!(e, ActionError::Cancelled);
        assert!(fake.calls().is_empty());
    }
}
