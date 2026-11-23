//! The host's checks on every call, before the workflow sees it (ADR 0092,
//! M4 spec §2): who may call the action, unknown and missing params, kinds,
//! choices, user-only params, and the scope rule: an `agent` param names
//! an agent the caller may see, else `forbidden:`. Workflows therefore
//! never re-implement scope.

use std::collections::{BTreeMap, HashMap};

use fleet_proto::param_value::Value;
use fleet_proto::{ActionCallers, ActionSpec, AgentInfo, ParamKind, ParamSpec, ParamValue};

use crate::error::ActionError;
use crate::host::{Caller, Host, Labels};
use crate::manifest::plain_absolute;

/// The label an agent's name is in (ADR 0052).
const NAME_LABEL: &str = "name";

/// A call's params after [`validate`]: every key is the action's, every
/// value of its kind, defaults filled in, `agent` params as ids.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params(BTreeMap<String, ParamValue>);

impl Params {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)?.value.as_ref()
    }
    /// A `string`, `text`, `choice` or `path` param.
    pub fn string(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// An `int` or `duration` (milliseconds) param.
    pub fn int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    /// An `agent` param: the agent's id.
    pub fn agent(&self, key: &str) -> Option<u64> {
        self.int(key).and_then(|i| u64::try_from(i).ok())
    }
    /// An `argv` param.
    pub fn list(&self, key: &str) -> Option<&[String]> {
        match self.get(key)? {
            Value::List(l) => Some(&l.items),
            _ => None,
        }
    }
    /// A `labels` param.
    pub fn map(&self, key: &str) -> Option<&HashMap<String, String>> {
        match self.get(key)? {
            Value::Map(m) => Some(&m.entries),
            _ => None,
        }
    }
    pub fn into_inner(self) -> BTreeMap<String, ParamValue> {
        self.0
    }
}

fn kind_name(kind: ParamKind) -> &'static str {
    match kind {
        ParamKind::Unspecified => "unspecified",
        ParamKind::ParamString => "string",
        ParamKind::ParamText => "text",
        ParamKind::ParamBool => "bool",
        ParamKind::ParamInt => "int",
        ParamKind::ParamDuration => "duration",
        ParamKind::ParamChoice => "choice",
        ParamKind::ParamPath => "path",
        ParamKind::ParamAgent => "agent",
        ParamKind::ParamArgv => "argv",
        ParamKind::ParamLabels => "labels",
    }
}

/// Checks a call of `spec` by `host`'s caller with `given` params (see the
/// module). `owned` is the workflow's owned label prefix: an agent may not
/// put keys of it in a `labels` param (only the workflow writes them).
pub async fn validate(
    spec: &ActionSpec,
    host: &dyn Host,
    owned: Option<&str>,
    given: HashMap<String, ParamValue>,
) -> Result<Params, ActionError> {
    let caller = host.caller();
    let agent_caller = match caller {
        Caller::User => None,
        Caller::Agent(a) => Some(a),
    };
    if let Some(a) = agent_caller
        && spec.callers() != ActionCallers::UserAndAgents
    {
        return Err(ActionError::forbidden(format!(
            "{} is the user's only (agent {a} called it)",
            spec.name
        )));
    }
    if let Some(key) = given
        .keys()
        .find(|k| !spec.params.iter().any(|p| &p.key == *k))
    {
        return Err(ActionError::invalid(format!(
            "{} has no param {key:?}",
            spec.name
        )));
    }
    // Listed once, and only when an agent param is given.
    let mut visible: Option<Vec<AgentInfo>> = None;
    let mut out = BTreeMap::new();
    for p in &spec.params {
        let Some(value) = given.get(&p.key) else {
            if let Some(default) = &p.default {
                out.insert(p.key.clone(), default.clone());
            } else if p.required {
                return Err(ActionError::invalid(format!(
                    "{} needs {:?}",
                    spec.name, p.key
                )));
            }
            continue;
        };
        if p.user_only
            && let Some(a) = agent_caller
        {
            return Err(ActionError::forbidden(format!(
                "{:?} of {} is the user's only (agent {a} set it)",
                p.key, spec.name
            )));
        }
        let value = check_value(p, value, agent_caller, owned, host, &mut visible).await?;
        out.insert(p.key.clone(), value);
    }
    Ok(Params(out))
}

async fn check_value(
    p: &ParamSpec,
    value: &ParamValue,
    agent_caller: Option<u64>,
    owned: Option<&str>,
    host: &dyn Host,
    visible: &mut Option<Vec<AgentInfo>>,
) -> Result<ParamValue, ActionError> {
    let kind = p.kind();
    let wrong = || ActionError::invalid(format!("{:?} must be a {} value", p.key, kind_name(kind)));
    let bad = |what: String| ActionError::invalid(format!("{:?}: {what}", p.key));
    let v = value.value.as_ref().ok_or_else(wrong)?;
    match (kind, v) {
        (ParamKind::ParamString | ParamKind::ParamText, Value::String(_))
        | (ParamKind::ParamBool, Value::Bool(_))
        | (ParamKind::ParamInt, Value::Int(_)) => Ok(value.clone()),
        (ParamKind::ParamDuration, Value::Int(ms)) if *ms >= 0 => Ok(value.clone()),
        (ParamKind::ParamDuration, Value::Int(_)) => Err(bad("a duration is not negative".into())),
        (ParamKind::ParamChoice, Value::String(s)) if p.choices.contains(s) => Ok(value.clone()),
        (ParamKind::ParamChoice, Value::String(s)) => {
            Err(bad(format!("{s:?} is not one of {}", p.choices.join(", "))))
        }
        (ParamKind::ParamPath, Value::String(s)) if plain_absolute(s) => Ok(value.clone()),
        (ParamKind::ParamPath, Value::String(s)) => {
            Err(bad(format!("{s:?} is not an absolute path without `..`")))
        }
        (ParamKind::ParamArgv, Value::List(l)) if !l.items.is_empty() => Ok(value.clone()),
        (ParamKind::ParamArgv, Value::List(_)) => Err(bad("the command line is empty".into())),
        (ParamKind::ParamLabels, Value::Map(m)) => {
            for key in m.entries.keys() {
                if key.starts_with("fleet.") {
                    return Err(bad(format!("label {key:?} is reserved")));
                }
                if let (Some(a), Some(prefix)) = (agent_caller, owned)
                    && key.starts_with(prefix)
                {
                    return Err(ActionError::forbidden(format!(
                        "label {key:?} is in prefix {prefix:?}, which only the workflow writes (agent {a} set it)"
                    )));
                }
            }
            Ok(value.clone())
        }
        (ParamKind::ParamAgent, Value::Int(_) | Value::String(_)) => {
            if visible.is_none() {
                *visible = Some(host.list_agents(Labels::new()).await?);
            }
            let agents = visible.as_deref().unwrap_or_default();
            let id = resolve_agent(&p.key, v, agents, agent_caller)?;
            Ok(ParamValue {
                value: Some(Value::Int(i64::try_from(id).map_err(|_| wrong())?)),
            })
        }
        _ => Err(wrong()),
    }
}

/// The id of the agent `v` names among `agents` (the caller's scope).
fn resolve_agent(
    key: &str,
    v: &Value,
    agents: &[AgentInfo],
    agent_caller: Option<u64>,
) -> Result<u64, ActionError> {
    let named = match v {
        Value::Int(i) => u64::try_from(*i).ok(),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    };
    let text = match v {
        Value::Int(i) => i.to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let found: Vec<u64> = match named {
        Some(id) => agents.iter().filter(|a| a.id == id).map(|a| a.id).collect(),
        None => agents
            .iter()
            .filter(|a| a.labels.get(NAME_LABEL) == Some(&text))
            .map(|a| a.id)
            .collect(),
    };
    match (found.as_slice(), agent_caller) {
        ([id], _) => Ok(*id),
        ([], Some(a)) => Err(ActionError::forbidden(format!(
            "{key:?}: agent {text} is not a descendant of agent {a}"
        ))),
        ([], None) => Err(ActionError::invalid(format!("{key:?}: no agent {text}"))),
        (ids, _) => Err(ActionError::invalid(format!(
            "{key:?}: {} agents are named {text:?}; give an id",
            ids.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeHost, agent};
    use crate::host::Caller;
    use fleet_proto::{ActionCallers, ActionSpec, ParamKind, ParamSpec, StringList, StringMap};
    use std::collections::HashMap;

    fn param(key: &str, kind: ParamKind) -> ParamSpec {
        ParamSpec {
            key: key.into(),
            kind: kind as i32,
            ..Default::default()
        }
    }

    fn spec() -> ActionSpec {
        let mut slug = param("slug", ParamKind::ParamString);
        slug.required = true;
        let mut against = param("against", ParamKind::ParamChoice);
        against.choices = vec!["base".into(), "uncommitted".into()];
        against.default = Some(s("base"));
        let mut force = param("force", ParamKind::ParamBool);
        force.user_only = true;
        ActionSpec {
            name: "wf.act".into(),
            callers: ActionCallers::UserAndAgents as i32,
            params: vec![
                slug,
                against,
                force,
                param("n", ParamKind::ParamInt),
                param("timeout", ParamKind::ParamDuration),
                param("repo", ParamKind::ParamPath),
                param("agent", ParamKind::ParamAgent),
                param("command", ParamKind::ParamArgv),
                param("labels", ParamKind::ParamLabels),
                param("note", ParamKind::ParamText),
            ],
            ..Default::default()
        }
    }

    fn s(v: &str) -> ParamValue {
        ParamValue {
            value: Some(Value::String(v.into())),
        }
    }
    fn b(v: bool) -> ParamValue {
        ParamValue {
            value: Some(Value::Bool(v)),
        }
    }
    fn i(v: i64) -> ParamValue {
        ParamValue {
            value: Some(Value::Int(v)),
        }
    }
    fn list(v: &[&str]) -> ParamValue {
        ParamValue {
            value: Some(Value::List(StringList {
                items: v.iter().map(|x| x.to_string()).collect(),
            })),
        }
    }
    fn map(v: &[(&str, &str)]) -> ParamValue {
        ParamValue {
            value: Some(Value::Map(StringMap {
                entries: v
                    .iter()
                    .map(|(k, x)| (k.to_string(), x.to_string()))
                    .collect(),
            })),
        }
    }

    fn given(pairs: Vec<(&str, ParamValue)>) -> HashMap<String, ParamValue> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    /// Agent 1 sees 2 ("kid") and 3 ("grandkid"); 5 ("other") is outside.
    fn agent_host() -> FakeHost {
        FakeHost::new(Caller::Agent(1)).with_agents(vec![agent(2, "kid"), agent(3, "grandkid")])
    }
    fn user_host() -> FakeHost {
        FakeHost::new(Caller::User).with_agents(vec![
            agent(2, "kid"),
            agent(3, "grandkid"),
            agent(5, "other"),
            agent(6, "twin"),
            agent(7, "twin"),
        ])
    }

    async fn check(host: &FakeHost, pairs: Vec<(&str, ParamValue)>) -> Result<Params, ActionError> {
        validate(&spec(), host, Some("git."), given(pairs)).await
    }

    #[tokio::test]
    async fn a_good_call_passes_with_defaults_filled_in() {
        let p = check(
            &user_host(),
            vec![
                ("slug", s("x")),
                ("force", b(true)),
                ("n", i(-3)),
                ("timeout", i(30_000)),
                ("repo", s("/r")),
                ("agent", s("kid")),
                ("command", list(&["claude", "-p"])),
                ("labels", map(&[("role", "rev"), ("git.base", "main")])),
                ("note", s("a\nb")),
            ],
        )
        .await
        .unwrap();
        assert_eq!(p.string("slug"), Some("x"));
        assert_eq!(p.string("against"), Some("base"), "the default");
        assert_eq!(p.bool("force"), Some(true));
        assert_eq!(p.int("n"), Some(-3));
        assert_eq!(p.int("timeout"), Some(30_000));
        assert_eq!(p.string("repo"), Some("/r"));
        assert_eq!(p.agent("agent"), Some(2), "a name becomes the id");
        assert_eq!(
            p.list("command"),
            Some(&["claude".to_string(), "-p".into()][..])
        );
        assert_eq!(
            p.map("labels").unwrap().get("role").map(String::as_str),
            Some("rev")
        );
        assert_eq!(p.string("note"), Some("a\nb"));
        assert_eq!(p.string("nothing"), None);
        assert_eq!(p.bool("slug"), None, "the wrong getter is None");
    }

    #[tokio::test]
    async fn missing_unknown_and_mistyped_params_are_invalid() {
        let host = user_host();
        let invalid = |r: Result<Params, ActionError>| matches!(r, Err(ActionError::Invalid(_)));
        assert!(invalid(check(&host, vec![]).await), "slug is required");
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("zzz", s("1"))]).await
        ));
        assert!(invalid(check(&host, vec![("slug", b(true))]).await));
        assert!(invalid(
            check(&host, vec![("slug", ParamValue { value: None })]).await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("force", s("true"))]).await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("n", s("1"))]).await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("timeout", i(-1))]).await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("against", s("all"))]).await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("repo", s("rel/dir"))]).await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("repo", s("/a/../b"))]).await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("command", list(&[]))]).await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("command", s("ls"))]).await
        ));
        assert!(invalid(
            check(
                &host,
                vec![("slug", s("x")), ("labels", map(&[("fleet.parent", "1")]))]
            )
            .await
        ));
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("agent", s("nobody"))]).await
        ));
        assert!(
            invalid(check(&host, vec![("slug", s("x")), ("agent", s("twin"))]).await),
            "ambiguous"
        );
        assert!(invalid(
            check(&host, vec![("slug", s("x")), ("agent", b(true))]).await
        ));
    }

    #[tokio::test]
    async fn the_user_may_name_any_agent_and_set_user_only_params() {
        let host = user_host();
        let p = check(
            &host,
            vec![("slug", s("x")), ("agent", i(5)), ("force", b(true))],
        )
        .await
        .unwrap();
        assert_eq!(p.agent("agent"), Some(5));
        let p = check(&host, vec![("slug", s("x")), ("agent", s("5"))])
            .await
            .unwrap();
        assert_eq!(p.agent("agent"), Some(5), "an id as a string");
    }

    #[tokio::test]
    async fn an_agent_names_only_its_descendants() {
        let host = agent_host();
        let p = check(&host, vec![("slug", s("x")), ("agent", s("grandkid"))])
            .await
            .unwrap();
        assert_eq!(p.agent("agent"), Some(3));
        for other in [i(5), s("5"), s("other"), i(1), s("999")] {
            let e = check(&host, vec![("slug", s("x")), ("agent", other.clone())])
                .await
                .unwrap_err();
            assert!(matches!(e, ActionError::Forbidden(_)), "{other:?}: {e}");
        }
    }

    #[tokio::test]
    async fn an_agent_may_not_set_user_only_params_or_the_owned_prefix() {
        let host = agent_host();
        let e = check(&host, vec![("slug", s("x")), ("force", b(true))])
            .await
            .unwrap_err();
        assert!(matches!(e, ActionError::Forbidden(_)), "{e}");
        // Even false: the agent may not set it at all.
        let e = check(&host, vec![("slug", s("x")), ("force", b(false))])
            .await
            .unwrap_err();
        assert!(matches!(e, ActionError::Forbidden(_)), "{e}");
        let e = check(
            &host,
            vec![
                ("slug", s("x")),
                ("labels", map(&[("git.worktree", "/etc")])),
            ],
        )
        .await
        .unwrap_err();
        assert!(matches!(e, ActionError::Forbidden(_)), "{e}");
        assert!(
            check(
                &host,
                vec![("slug", s("x")), ("labels", map(&[("role", "r")]))]
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn user_only_actions_refuse_agents() {
        let mut spec = spec();
        spec.callers = ActionCallers::User as i32;
        let e = validate(&spec, &agent_host(), None, given(vec![("slug", s("x"))]))
            .await
            .unwrap_err();
        assert!(matches!(e, ActionError::Forbidden(_)), "{e}");
        assert!(
            validate(&spec, &user_host(), None, given(vec![("slug", s("x"))]))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn agents_are_listed_only_when_an_agent_param_is_given() {
        let host = user_host();
        check(&host, vec![("slug", s("x"))]).await.unwrap();
        assert!(host.calls().is_empty());
    }
}
