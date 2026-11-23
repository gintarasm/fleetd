//! The generic action form (ADR 0094): an [`ActionSpec`]'s params as
//! fields the Run Action… sheet draws, their validation, and the
//! [`ParamValue`]s a `RunAction` sends. Every workflow's actions get a form
//! from their schema alone, with no GUI code of their own.

use std::collections::HashMap;

use fleet_proto::param_value::Value;
use fleet_proto::{ActionSpec, ParamKind, ParamSpec, ParamValue, StringList, StringMap};

/// How a field is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// A one-line text field (string, int, duration, argv, labels).
    Line,
    /// A multi-line text field.
    Lines,
    /// A checkbox.
    Check,
    /// A pop-up of these choices.
    Choice(Vec<String>),
    /// An agent picker (id or name).
    Agent,
    /// A folder on the node.
    Folder,
}

/// One field of the form.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub key: String,
    /// The param's title, else its key.
    pub title: String,
    pub help: String,
    pub placeholder: String,
    pub required: bool,
    pub kind: ParamKind,
    pub input: Input,
    /// What is typed (or the chosen choice); unused for a checkbox.
    pub text: String,
    /// A checkbox's state.
    pub checked: bool,
    /// The default the param had, so an untouched field sends nothing.
    default: Option<Value>,
}

/// A form for one action.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionForm {
    pub name: String,
    pub title: String,
    pub description: String,
    pub destructive: bool,
    pub fields: Vec<Field>,
}

/// What is wrong with a field, by its index in [`ActionForm::fields`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: usize,
    pub message: String,
}

impl ActionForm {
    pub fn new(spec: &ActionSpec) -> ActionForm {
        ActionForm {
            name: spec.name.clone(),
            title: if spec.title.is_empty() {
                spec.name.clone()
            } else {
                spec.title.clone()
            },
            description: spec.description.clone(),
            destructive: spec.destructive,
            fields: spec.params.iter().map(field).collect(),
        }
    }

    /// The field for `key`.
    pub fn field(&self, key: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.key == key)
    }

    /// Fills the action's target param (its `agent` param) with an agent,
    /// for an agent's context menu.
    pub fn target(&mut self, spec: &ActionSpec, agent_id: u64) {
        if let Some(i) = self.field(&spec.target_param) {
            self.fields[i].text = agent_id.to_string();
        }
    }

    /// The params to send, or what to fix. `home` expands a leading `~`
    /// in a folder.
    pub fn params(&self, home: &str) -> Result<HashMap<String, ParamValue>, Vec<FieldError>> {
        let mut out = HashMap::new();
        let mut errors = Vec::new();
        for (i, f) in self.fields.iter().enumerate() {
            match value(f, home) {
                Ok(Some(v)) => {
                    out.insert(f.key.clone(), ParamValue { value: Some(v) });
                }
                Ok(None) => {}
                Err(message) => errors.push(FieldError { field: i, message }),
            }
        }
        if errors.is_empty() {
            Ok(out)
        } else {
            Err(errors)
        }
    }
}

fn field(p: &ParamSpec) -> Field {
    let kind = p.kind();
    let input = match kind {
        ParamKind::ParamText => Input::Lines,
        ParamKind::ParamBool => Input::Check,
        ParamKind::ParamChoice => Input::Choice(p.choices.clone()),
        ParamKind::ParamAgent => Input::Agent,
        ParamKind::ParamPath => Input::Folder,
        _ => Input::Line,
    };
    let default = p.default.as_ref().and_then(|d| d.value.clone());
    let (text, checked) = match &default {
        Some(Value::Bool(b)) => (String::new(), *b),
        Some(v) => (default_text(kind, v), false),
        None => (String::new(), false),
    };
    Field {
        key: p.key.clone(),
        title: if p.title.is_empty() {
            p.key.clone()
        } else {
            p.title.clone()
        },
        help: p.help.clone(),
        placeholder: p.placeholder.clone(),
        required: p.required,
        kind,
        input,
        text,
        checked,
        default,
    }
}

/// A default as the field shows it.
fn default_text(kind: ParamKind, v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) if kind == ParamKind::ParamDuration && n % 1000 == 0 => {
            format!("{}s", n / 1000)
        }
        Value::Int(n) if kind == ParamKind::ParamDuration => format!("{n}ms"),
        Value::Int(n) => n.to_string(),
        Value::List(l) => shell_words::join(&l.items),
        Value::Map(m) => {
            let mut pairs: Vec<_> = m.entries.iter().map(|(k, v)| format!("{k}={v}")).collect();
            pairs.sort();
            shell_words::join(pairs)
        }
    }
}

/// The field's value, `None` when nothing is to be sent.
fn value(f: &Field, home: &str) -> Result<Option<Value>, String> {
    if f.kind == ParamKind::ParamBool {
        let v = Value::Bool(f.checked);
        let unchanged = f.default.as_ref().unwrap_or(&Value::Bool(false)) == &v;
        return Ok((f.required || !unchanged).then_some(v));
    }
    let text = if matches!(f.kind, ParamKind::ParamString | ParamKind::ParamText) {
        f.text.as_str()
    } else {
        f.text.trim()
    };
    if text.trim().is_empty() {
        return if f.required {
            Err("Required".into())
        } else {
            Ok(None)
        };
    }
    let v = match f.kind {
        ParamKind::ParamInt => Value::Int(text.parse().map_err(|_| "Not a whole number")?),
        ParamKind::ParamDuration => {
            Value::Int(parse_duration(text).ok_or("Not a duration (like 30s, 5m, 250ms)")?)
        }
        ParamKind::ParamChoice => match &f.input {
            Input::Choice(c) if !c.iter().any(|x| x == text) => {
                return Err(format!("Not one of: {}", c.join(", ")));
            }
            _ => Value::String(text.to_owned()),
        },
        ParamKind::ParamPath => {
            let path = match text.strip_prefix('~') {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
                _ => text.to_owned(),
            };
            if !path.starts_with('/') {
                return Err("Not a full path (start with / or ~)".into());
            }
            Value::String(path)
        }
        ParamKind::ParamAgent => match text.parse::<i64>() {
            Ok(id) => Value::Int(id),
            Err(_) => Value::String(text.to_owned()),
        },
        ParamKind::ParamArgv => {
            let items = shell_words::split(text).map_err(|_| "Unclosed quote")?;
            Value::List(StringList { items })
        }
        ParamKind::ParamLabels => {
            let words = shell_words::split(text).map_err(|_| "Unclosed quote")?;
            let mut entries = HashMap::new();
            for w in words {
                match w.split_once('=') {
                    Some((k, v)) if !k.is_empty() => {
                        entries.insert(k.to_owned(), v.to_owned());
                    }
                    _ => return Err(format!("Not key=value: {w}")),
                }
            }
            Value::Map(StringMap { entries })
        }
        _ => Value::String(text.to_owned()),
    };
    if f.default.as_ref() == Some(&v) && !f.required {
        return Ok(None);
    }
    Ok(Some(v))
}

/// `params` with a refusal's `retry` merged in (its values win): what
/// "… Anyway" resends.
pub fn with_retry(
    mut params: HashMap<String, ParamValue>,
    retry: &HashMap<String, ParamValue>,
) -> HashMap<String, ParamValue> {
    for (k, v) in retry {
        params.insert(k.clone(), v.clone());
    }
    params
}

/// Milliseconds from `250ms`, `30s`, `5m`, `1h`, or bare seconds (as the
/// CLI reads them).
pub fn parse_duration(s: &str) -> Option<i64> {
    let (digits, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: i64 = digits.parse().ok()?;
    let scale = match unit.trim() {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return None,
    };
    n.checked_mul(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(key: &str, kind: ParamKind) -> ParamSpec {
        ParamSpec {
            key: key.into(),
            kind: kind as i32,
            ..Default::default()
        }
    }

    fn spec(params: Vec<ParamSpec>) -> ActionSpec {
        ActionSpec {
            name: "w.act".into(),
            title: "Act".into(),
            description: "Does it".into(),
            params,
            ..Default::default()
        }
    }

    fn string(s: &str) -> ParamValue {
        ParamValue {
            value: Some(Value::String(s.into())),
        }
    }

    fn val(p: &HashMap<String, ParamValue>, k: &str) -> Option<Value> {
        p.get(k).and_then(|v| v.value.clone())
    }

    #[test]
    fn kinds_map_to_inputs() {
        let mut choice = param("against", ParamKind::ParamChoice);
        choice.choices = vec!["base".into(), "uncommitted".into()];
        let f = ActionForm::new(&spec(vec![
            param("s", ParamKind::ParamString),
            param("t", ParamKind::ParamText),
            param("b", ParamKind::ParamBool),
            choice,
            param("a", ParamKind::ParamAgent),
            param("p", ParamKind::ParamPath),
            param("argv", ParamKind::ParamArgv),
        ]));
        let inputs: Vec<_> = f.fields.iter().map(|f| f.input.clone()).collect();
        assert_eq!(
            inputs,
            [
                Input::Line,
                Input::Lines,
                Input::Check,
                Input::Choice(vec!["base".into(), "uncommitted".into()]),
                Input::Agent,
                Input::Folder,
                Input::Line,
            ]
        );
        assert_eq!(f.fields[0].title, "s", "no title: the key");
        assert_eq!(f.name, "w.act");
    }

    #[test]
    fn defaults_fill_fields_and_untouched_ones_send_nothing() {
        let mut ctx = param("context", ParamKind::ParamInt);
        ctx.default = Some(ParamValue {
            value: Some(Value::Int(3)),
        });
        let mut keep = param("keep", ParamKind::ParamBool);
        keep.default = Some(ParamValue {
            value: Some(Value::Bool(true)),
        });
        let mut against = param("against", ParamKind::ParamChoice);
        against.choices = vec!["base".into(), "committed".into()];
        against.default = Some(string("base"));
        let f = ActionForm::new(&spec(vec![ctx, keep, against]));
        assert_eq!(f.fields[0].text, "3");
        assert!(f.fields[1].checked);
        assert_eq!(f.fields[2].text, "base");
        assert!(f.params("/h").unwrap().is_empty());
    }

    #[test]
    fn changed_fields_are_sent_by_kind() {
        let mut f = ActionForm::new(&spec(vec![
            param("s", ParamKind::ParamString),
            param("n", ParamKind::ParamInt),
            param("d", ParamKind::ParamDuration),
            param("b", ParamKind::ParamBool),
            param("argv", ParamKind::ParamArgv),
            param("labels", ParamKind::ParamLabels),
            param("p", ParamKind::ParamPath),
            param("a", ParamKind::ParamAgent),
            param("a2", ParamKind::ParamAgent),
        ]));
        let set = |f: &mut ActionForm, k: &str, t: &str| {
            let i = f.field(k).unwrap();
            f.fields[i].text = t.into();
        };
        set(&mut f, "s", " hi ");
        set(&mut f, "n", "42");
        set(&mut f, "d", "5m");
        let b = f.field("b").unwrap();
        f.fields[b].checked = true;
        set(&mut f, "argv", "claude --model 'big one'");
        set(&mut f, "labels", "team=core m4=d");
        set(&mut f, "p", "~/src");
        set(&mut f, "a", "17");
        set(&mut f, "a2", "m4-d");
        let p = f.params("/Users/me").unwrap();
        assert_eq!(val(&p, "s"), Some(Value::String(" hi ".into())));
        assert_eq!(val(&p, "n"), Some(Value::Int(42)));
        assert_eq!(val(&p, "d"), Some(Value::Int(300_000)));
        assert_eq!(val(&p, "b"), Some(Value::Bool(true)));
        assert_eq!(
            val(&p, "argv"),
            Some(Value::List(StringList {
                items: vec!["claude".into(), "--model".into(), "big one".into()]
            }))
        );
        assert_eq!(
            val(&p, "labels"),
            Some(Value::Map(StringMap {
                entries: HashMap::from([("team".into(), "core".into()), ("m4".into(), "d".into())])
            }))
        );
        assert_eq!(val(&p, "p"), Some(Value::String("/Users/me/src".into())));
        assert_eq!(val(&p, "a"), Some(Value::Int(17)));
        assert_eq!(val(&p, "a2"), Some(Value::String("m4-d".into())));
    }

    #[test]
    fn bad_values_name_their_field() {
        let mut req = param("slug", ParamKind::ParamString);
        req.required = true;
        let mut choice = param("c", ParamKind::ParamChoice);
        choice.choices = vec!["x".into()];
        let mut f = ActionForm::new(&spec(vec![
            req,
            param("n", ParamKind::ParamInt),
            param("d", ParamKind::ParamDuration),
            param("p", ParamKind::ParamPath),
            param("labels", ParamKind::ParamLabels),
            param("argv", ParamKind::ParamArgv),
            choice,
        ]));
        f.fields[1].text = "four".into();
        f.fields[2].text = "5 parsecs".into();
        f.fields[3].text = "relative/dir".into();
        f.fields[4].text = "novalue".into();
        f.fields[5].text = "unclosed 'quote".into();
        f.fields[6].text = "y".into();
        let errors = f.params("/h").unwrap_err();
        let fields: Vec<_> = errors.iter().map(|e| e.field).collect();
        assert_eq!(fields, [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(errors[0].message, "Required");
    }

    #[test]
    fn a_required_bool_is_always_sent() {
        let mut b = param("yes", ParamKind::ParamBool);
        b.required = true;
        let f = ActionForm::new(&spec(vec![b]));
        assert_eq!(
            val(&f.params("/h").unwrap(), "yes"),
            Some(Value::Bool(false))
        );
    }

    #[test]
    fn the_target_param_takes_the_agent() {
        let mut s = spec(vec![param("agent", ParamKind::ParamAgent)]);
        s.target_param = "agent".into();
        let mut f = ActionForm::new(&s);
        f.target(&s, 9);
        assert_eq!(val(&f.params("/h").unwrap(), "agent"), Some(Value::Int(9)));
    }

    #[test]
    fn retry_values_win() {
        let params = HashMap::from([(
            "force".to_owned(),
            ParamValue {
                value: Some(Value::Bool(false)),
            },
        )]);
        let retry = HashMap::from([(
            "force".to_owned(),
            ParamValue {
                value: Some(Value::Bool(true)),
            },
        )]);
        let p = with_retry(params, &retry);
        assert_eq!(val(&p, "force"), Some(Value::Bool(true)));
    }

    #[test]
    fn durations_read_like_the_cli() {
        assert_eq!(parse_duration("250ms"), Some(250));
        assert_eq!(parse_duration("30"), Some(30_000));
        assert_eq!(parse_duration("2h"), Some(7_200_000));
        assert_eq!(parse_duration("x"), None);
        assert_eq!(parse_duration("3d"), None);
    }
}
