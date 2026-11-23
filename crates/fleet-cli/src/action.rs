//! `fleet action` (ADR 0092): list the daemon's workflow actions and run
//! one. Params are `key=value`, parsed by the param's kind; an `argv` param
//! takes the words after `--`. Views print as text: a diff as a unified
//! diff, a table as aligned columns. Exit codes: 0 ok, 1 refused (the
//! reason and the params to override it go to stderr), 2 usage or other
//! error, 3 forbidden.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use clap::{Args as ClapArgs, Subcommand};
use fleet_client::{Connection, Error, Result};
use fleet_proto::action_result::Outcome;
use fleet_proto::param_value::Value;
use fleet_proto::{
    ActionCallers, ActionRefused, ActionSpec, DiffView, FileStatus, ListActions, ParamKind,
    ParamSpec, ParamValue, RunAction, StringList, StringMap, TableView, View, request, response,
    view,
};

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List the actions you may run
    Ls {
        /// Print JSON (with each action's params) instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Run an action: `fleet action run <name> [key=value]... [-- argv...]`
    Run {
        /// The action, as `<workflow>.<action>`
        name: String,
        /// Params as key=value (a bare key sets a bool; labels=k=v repeats;
        /// durations take ms, s, m or h, bare = seconds; relative paths are
        /// made absolute)
        params: Vec<String>,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
        /// The command line, for the action's `argv` param
        #[arg(last = true)]
        argv: Vec<String>,
    },
}

/// Runs `fleet action`; the exit code.
pub fn run(client: &mut Connection, args: Args) -> Result<u8> {
    let outcome = match args.cmd {
        Cmd::Ls { json } => list(client).map(|actions| {
            if json {
                let items: Vec<_> = actions.iter().map(spec_json).collect();
                println!("{}", serde_json::Value::Array(items));
            } else {
                print!("{}", render_list(&actions));
            }
            0
        }),
        Cmd::Run {
            name,
            params,
            json,
            argv,
        } => run_action(client, &name, &params, argv, json),
    };
    match outcome {
        Ok(code) => Ok(code),
        // Exit 3, from `main`.
        Err(e) if e.is_forbidden() => Err(e),
        Err(e) => {
            eprintln!("fleet: {e}");
            Ok(2)
        }
    }
}

fn list(client: &mut Connection) -> Result<Vec<ActionSpec>> {
    match client.call(request::Kind::ListActions(ListActions {})) {
        Ok(response::Kind::ActionList(l)) => Ok(l.actions),
        Ok(_) => Err(Error::Protocol("unexpected reply to list actions".into())),
        // An M3 daemon does not know the request.
        Err(Error::Server(m)) if m == "empty request" => Err(Error::Server(
            "the daemon is too old for workflow actions".into(),
        )),
        Err(e) => Err(e),
    }
}

fn run_action(
    client: &mut Connection,
    name: &str,
    params: &[String],
    argv: Vec<String>,
    json: bool,
) -> Result<u8> {
    let actions = list(client)?;
    let Some(spec) = actions.iter().find(|a| a.name == name) else {
        return Err(Error::Server(format!(
            "no action {name:?} (see `fleet action ls`)"
        )));
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let params = parse_params(spec, params, argv, &cwd).map_err(Error::Server)?;
    let req = RunAction {
        name: name.to_owned(),
        params,
        cwd: cwd.to_string_lossy().into_owned(),
    };
    let outcome = match client.call(request::Kind::RunAction(Box::new(req)))? {
        response::Kind::ActionResult(r) => r
            .outcome
            .ok_or_else(|| Error::Protocol("an action result without an outcome".into()))?,
        _ => return Err(Error::Protocol("unexpected reply to run action".into())),
    };
    if json {
        println!("{}", outcome_json(&outcome));
        return Ok(u8::from(matches!(outcome, Outcome::Refused(_))));
    }
    Ok(match outcome {
        Outcome::Done(d) => {
            if !d.message.is_empty() {
                println!("{}", d.message);
            }
            0
        }
        Outcome::Spawned(s) => {
            if let Some(agent) = &s.agent {
                println!("{}", agent.id);
            }
            if !s.message.is_empty() {
                eprintln!("{}", s.message);
            }
            0
        }
        Outcome::View(v) => {
            print!("{}", render_view(&v));
            0
        }
        Outcome::Refused(r) => {
            eprintln!("fleet: refused: {}", r.reason);
            let hint = retry_hint(&r);
            if !hint.is_empty() {
                eprintln!("fleet: to do it anyway, add: {hint}");
            }
            1
        }
    })
}

/// The params `args` give for `spec`, each parsed by its kind; `argv` (the
/// words after `--`) goes to the action's `argv` param.
pub fn parse_params(
    spec: &ActionSpec,
    args: &[String],
    argv: Vec<String>,
    cwd: &Path,
) -> std::result::Result<HashMap<String, ParamValue>, String> {
    let find = |key: &str| {
        spec.params
            .iter()
            .find(|p| p.key == key)
            .ok_or_else(|| format!("{} has no param {key:?}", spec.name))
    };
    let mut out: HashMap<String, ParamValue> = HashMap::new();
    for arg in args {
        let (key, value) = match arg.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (arg.as_str(), None),
        };
        let p = find(key)?;
        let parsed = parse_value(p, value, cwd)?;
        match (out.get_mut(key), parsed) {
            (
                Some(ParamValue {
                    value: Some(Value::Map(have)),
                }),
                Value::Map(more),
            ) => have.entries.extend(more.entries),
            (Some(_), _) => return Err(format!("{key:?} is given twice")),
            (None, v) => {
                out.insert(key.to_owned(), ParamValue { value: Some(v) });
            }
        }
    }
    if !argv.is_empty() {
        let p = spec
            .params
            .iter()
            .find(|p| p.kind() == ParamKind::ParamArgv)
            .ok_or_else(|| format!("{} takes no command after --", spec.name))?;
        out.insert(
            p.key.clone(),
            ParamValue {
                value: Some(Value::List(StringList { items: argv })),
            },
        );
    }
    Ok(out)
}

fn parse_value(
    p: &ParamSpec,
    value: Option<&str>,
    cwd: &Path,
) -> std::result::Result<Value, String> {
    let key = &p.key;
    if p.kind() == ParamKind::ParamBool {
        return match value {
            None | Some("true" | "yes" | "1") => Ok(Value::Bool(true)),
            Some("false" | "no" | "0") => Ok(Value::Bool(false)),
            Some(v) => Err(format!("{key:?} is true or false, not {v:?}")),
        };
    }
    let Some(v) = value else {
        return Err(format!("{key:?} needs a value: {key}=..."));
    };
    Ok(match p.kind() {
        ParamKind::ParamInt => Value::Int(
            v.parse()
                .map_err(|_| format!("{key:?} is an integer, not {v:?}"))?,
        ),
        ParamKind::ParamDuration => {
            Value::Int(parse_duration(v).map_err(|e| format!("{key:?}: {e}"))?)
        }
        ParamKind::ParamPath => Value::String(absolute(cwd, v)),
        ParamKind::ParamLabels => {
            let (k, val) = v
                .split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .ok_or_else(|| format!("{key:?} takes k=v, as {key}=k=v"))?;
            Value::Map(StringMap {
                entries: HashMap::from([(k.to_owned(), val.to_owned())]),
            })
        }
        ParamKind::ParamArgv => {
            return Err(format!("{key:?} is a command line: give it after --"));
        }
        _ => Value::String(v.to_owned()),
    })
}

/// Milliseconds from `250ms`, `30s`, `5m`, `1h`, or bare seconds.
pub fn parse_duration(s: &str) -> std::result::Result<i64, String> {
    let (digits, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: i64 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a duration (like 30s, 5m, 250ms)"))?;
    let scale = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err(format!("{s:?} is not a duration (like 30s, 5m, 250ms)")),
    };
    n.checked_mul(scale)
        .ok_or_else(|| format!("{s:?} is too long"))
}

/// `path` against `cwd`, with `.` and `..` resolved by name (the daemon
/// takes plain absolute paths only).
pub fn absolute(cwd: &Path, path: &str) -> String {
    let mut out = PathBuf::from("/");
    for c in cwd.join(path).components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out.to_string_lossy().into_owned()
}

fn kind_name(kind: ParamKind) -> &'static str {
    match kind {
        ParamKind::Unspecified => "?",
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

fn callers_name(spec: &ActionSpec) -> &'static str {
    match spec.callers() {
        ActionCallers::User => "user",
        ActionCallers::UserAndAgents => "user+agents",
    }
}

/// `ls`: one row per action, required params starred.
fn render_list(actions: &[ActionSpec]) -> String {
    let rows: Vec<[String; 4]> = std::iter::once([
        "NAME".to_string(),
        "WHO".into(),
        "PARAMS".into(),
        "TITLE".into(),
    ])
    .chain(actions.iter().map(|a| {
        let params = a
            .params
            .iter()
            .map(|p| {
                let star = if p.required { "*" } else { "" };
                if p.kind() == ParamKind::ParamArgv {
                    format!("-- {}{star}", p.key)
                } else {
                    format!("{}{star}", p.key)
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        [
            a.name.clone(),
            callers_name(a).into(),
            params,
            a.title.clone(),
        ]
    }))
    .collect();
    let width = |c: usize| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0);
    let (w0, w1, w2) = (width(0), width(1), width(2));
    let mut out = String::new();
    for r in &rows {
        let line = format!("{:<w0$}  {:<w1$}  {:<w2$}  {}", r[0], r[1], r[2], r[3]);
        let _ = writeln!(out, "{}", line.trim_end());
    }
    out
}

fn value_json(v: &ParamValue) -> serde_json::Value {
    match &v.value {
        None => serde_json::Value::Null,
        Some(Value::String(s)) => s.clone().into(),
        Some(Value::Bool(b)) => (*b).into(),
        Some(Value::Int(i)) => (*i).into(),
        Some(Value::List(l)) => l.items.clone().into(),
        Some(Value::Map(m)) => serde_json::json!(m.entries.iter().collect::<BTreeMap<_, _>>()),
    }
}

fn spec_json(a: &ActionSpec) -> serde_json::Value {
    serde_json::json!({
        "name": a.name,
        "title": a.title,
        "description": a.description,
        "callers": callers_name(a),
        "destructive": a.destructive,
        "target_param": a.target_param,
        "params": a.params.iter().map(|p| serde_json::json!({
            "key": p.key,
            "title": p.title,
            "kind": kind_name(p.kind()),
            "required": p.required,
            "default": p.default.as_ref().map(value_json),
            "choices": p.choices,
            "help": p.help,
            "placeholder": p.placeholder,
            "user_only": p.user_only,
        })).collect::<Vec<_>>(),
    })
}

fn params_json(m: &HashMap<String, ParamValue>) -> serde_json::Value {
    serde_json::json!(
        m.iter()
            .map(|(k, v)| (k, value_json(v)))
            .collect::<BTreeMap<_, _>>()
    )
}

fn status_name(s: FileStatus) -> &'static str {
    match s {
        FileStatus::Unspecified => "unknown",
        FileStatus::FileAdded => "added",
        FileStatus::FileModified => "modified",
        FileStatus::FileDeleted => "deleted",
        FileStatus::FileRenamed => "renamed",
        FileStatus::FileCopied => "copied",
        FileStatus::FileTypeChanged => "type_changed",
    }
}

fn view_json(v: &View) -> serde_json::Value {
    match &v.kind {
        None => serde_json::Value::Null,
        Some(view::Kind::Text(t)) => {
            serde_json::json!({"text": {"title": t.title, "text": t.text}})
        }
        Some(view::Kind::Table(t)) => serde_json::json!({"table": {
            "title": t.title,
            "columns": t.columns,
            "rows": t.rows.iter().map(|r| serde_json::json!({
                "cells": r.cells, "agent_id": r.agent_id, "path": r.path,
            })).collect::<Vec<_>>(),
        }}),
        Some(view::Kind::Diff(d)) => serde_json::json!({"diff": {
            "title": d.title,
            "from_label": d.from_label,
            "to_label": d.to_label,
            "files_changed": d.files_changed,
            "additions": d.additions,
            "deletions": d.deletions,
            "truncated": d.truncated,
            "files": d.files.iter().map(|f| serde_json::json!({
                "old_path": f.old_path,
                "new_path": f.new_path,
                "status": status_name(f.status()),
                "binary": f.binary,
                "additions": f.additions,
                "deletions": f.deletions,
                "hunks_omitted": f.hunks_omitted,
                "old_mode": f.old_mode,
                "new_mode": f.new_mode,
                "hunks": f.hunks.iter().map(|h| serde_json::json!({
                    "old_start": h.old_start, "old_lines": h.old_lines,
                    "new_start": h.new_start, "new_lines": h.new_lines,
                    "header": h.header,
                    "body": String::from_utf8_lossy(&h.body),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }}),
    }
}

pub fn outcome_json(o: &Outcome) -> serde_json::Value {
    match o {
        Outcome::Done(d) => serde_json::json!({"done": {"message": d.message}}),
        Outcome::Spawned(s) => serde_json::json!({"spawned": {
            "id": s.agent.as_ref().map(|a| a.id),
            "labels": s.agent.as_ref().map(|a| a.labels.iter().collect::<BTreeMap<_, _>>()),
            "cwd": s.agent.as_ref().map(|a| a.cwd.clone()),
            "message": s.message,
        }}),
        Outcome::View(v) => serde_json::json!({"view": view_json(v)}),
        Outcome::Refused(r) => serde_json::json!({"refused": {
            "reason": r.reason,
            "retry": params_json(&r.retry),
        }}),
    }
}

/// What to add to the command line to override a refusal: `force=true`.
pub fn retry_hint(r: &ActionRefused) -> String {
    let sorted: BTreeMap<_, _> = r.retry.iter().collect();
    sorted
        .into_iter()
        .filter_map(|(k, v)| {
            let v = match v.value.as_ref()? {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Int(i) => i.to_string(),
                Value::List(l) => l.items.join(" "),
                Value::Map(m) => m
                    .entries
                    .iter()
                    .map(|(a, b)| format!("{a}={b}"))
                    .collect::<Vec<_>>()
                    .join(","),
            };
            Some(format!("{k}={v}"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn render_view(v: &View) -> String {
    match &v.kind {
        None => String::new(),
        Some(view::Kind::Diff(d)) => render_diff(d),
        Some(view::Kind::Table(t)) => render_table(t),
        Some(view::Kind::Text(t)) if t.text.ends_with('\n') || t.text.is_empty() => t.text.clone(),
        Some(view::Kind::Text(t)) => format!("{}\n", t.text),
    }
}

/// A hunk's range as git writes it: the count only when it is not 1.
fn range(start: u32, lines: u32) -> String {
    if lines == 1 {
        start.to_string()
    } else {
        format!("{start},{lines}")
    }
}

/// A `DiffView` as a unified diff, as `git diff` prints it.
pub fn render_diff(d: &DiffView) -> String {
    let mut out = String::new();
    for f in &d.files {
        let old = if f.old_path.is_empty() {
            &f.new_path
        } else {
            &f.old_path
        };
        let new = if f.new_path.is_empty() {
            &f.old_path
        } else {
            &f.new_path
        };
        let _ = writeln!(out, "diff --git a/{old} b/{new}");
        let mode = |m: &str| {
            if m.is_empty() {
                String::new()
            } else {
                format!(" mode {m}")
            }
        };
        match f.status() {
            FileStatus::FileAdded => {
                let _ = writeln!(out, "new file{}", mode(&f.new_mode));
            }
            FileStatus::FileDeleted => {
                let _ = writeln!(out, "deleted file{}", mode(&f.old_mode));
            }
            FileStatus::FileRenamed | FileStatus::FileCopied => {
                let verb = if f.status() == FileStatus::FileRenamed {
                    "rename"
                } else {
                    "copy"
                };
                let _ = writeln!(out, "{verb} from {old}\n{verb} to {new}");
            }
            _ => {}
        }
        if !f.old_mode.is_empty() && !f.new_mode.is_empty() && f.old_mode != f.new_mode {
            let _ = writeln!(out, "old mode {}\nnew mode {}", f.old_mode, f.new_mode);
        }
        let a = if f.old_path.is_empty() {
            "/dev/null".to_string()
        } else {
            format!("a/{old}")
        };
        let b = if f.new_path.is_empty() {
            "/dev/null".to_string()
        } else {
            format!("b/{new}")
        };
        if f.binary {
            let _ = writeln!(out, "Binary files {a} and {b} differ");
            continue;
        }
        if f.hunks_omitted {
            let _ = writeln!(
                out,
                "# +{} -{}: hunks omitted; ask again with file={new}",
                f.additions, f.deletions
            );
            continue;
        }
        if f.hunks.is_empty() {
            continue;
        }
        let _ = writeln!(out, "--- {a}\n+++ {b}");
        for h in &f.hunks {
            let header = if h.header.is_empty() {
                String::new()
            } else {
                format!(" {}", h.header)
            };
            let _ = writeln!(
                out,
                "@@ -{} +{} @@{header}",
                range(h.old_start, h.old_lines),
                range(h.new_start, h.new_lines)
            );
            let body = String::from_utf8_lossy(&h.body);
            out.push_str(&body);
            if !body.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// A `TableView` as aligned columns under a header.
pub fn render_table(t: &TableView) -> String {
    let n = t
        .columns
        .len()
        .max(t.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0));
    let rows: Vec<Vec<&str>> = std::iter::once(&t.columns)
        .chain(t.rows.iter().map(|r| &r.cells))
        .map(|cells| {
            (0..n)
                .map(|i| cells.get(i).map_or("", String::as_str))
                .collect()
        })
        .collect();
    let widths: Vec<usize> = (0..n)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for r in &rows {
        let mut line = String::new();
        for (i, cell) in r.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let _ = write!(line, "{cell:<w$}", w = widths[i]);
        }
        let _ = writeln!(out, "{}", line.trim_end());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::{
        ActionRefused, DiffView, FileDiff, FileStatus, Hunk, ParamKind, ParamSpec, TableRow,
        TableView, TextView, View, view,
    };
    use std::path::Path;

    fn p(key: &str, kind: ParamKind) -> ParamSpec {
        ParamSpec {
            key: key.into(),
            kind: kind as i32,
            ..Default::default()
        }
    }

    fn spec() -> ActionSpec {
        ActionSpec {
            name: "w.a".into(),
            params: vec![
                p("slug", ParamKind::ParamString),
                p("force", ParamKind::ParamBool),
                p("n", ParamKind::ParamInt),
                p("timeout", ParamKind::ParamDuration),
                p("repo", ParamKind::ParamPath),
                p("agent", ParamKind::ParamAgent),
                p("command", ParamKind::ParamArgv),
                p("labels", ParamKind::ParamLabels),
                p("against", ParamKind::ParamChoice),
            ],
            ..Default::default()
        }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn val(v: Value) -> ParamValue {
        ParamValue { value: Some(v) }
    }

    #[test]
    fn params_parse_by_kind() {
        let got = parse_params(
            &spec(),
            &args(&[
                "slug=a=b",
                "force",
                "n=-4",
                "timeout=2m",
                "repo=sub/../dir",
                "agent=kid",
                "labels=role=rev",
                "labels=team=x",
                "against=base",
            ]),
            args(&["claude", "-p", "hi"]),
            Path::new("/home/u"),
        )
        .unwrap();
        assert_eq!(got["slug"], val(Value::String("a=b".into())));
        assert_eq!(got["force"], val(Value::Bool(true)));
        assert_eq!(got["n"], val(Value::Int(-4)));
        assert_eq!(got["timeout"], val(Value::Int(120_000)));
        assert_eq!(got["repo"], val(Value::String("/home/u/dir".into())));
        assert_eq!(got["agent"], val(Value::String("kid".into())));
        assert_eq!(
            got["command"],
            val(Value::List(StringList {
                items: args(&["claude", "-p", "hi"])
            }))
        );
        let Some(Value::Map(m)) = &got["labels"].value else {
            panic!()
        };
        assert_eq!(m.entries.len(), 2);
        assert_eq!(got["against"], val(Value::String("base".into())));
    }

    #[test]
    fn bad_params_are_usage_errors() {
        let bad = |a: &[&str], argv: &[&str]| {
            parse_params(&spec(), &args(a), args(argv), Path::new("/")).is_err()
        };
        assert!(bad(&["nope=1"], &[]), "unknown key");
        assert!(bad(&["slug"], &[]), "a non-bool needs a value");
        assert!(bad(&["force=maybe"], &[]));
        assert!(bad(&["n=x"], &[]));
        assert!(bad(&["timeout=5y"], &[]));
        assert!(bad(&["slug=a", "slug=b"], &[]), "repeated");
        assert!(bad(&["labels=novalue"], &[]));
        assert!(bad(&["command=ls"], &[]), "argv comes after --");
        let no_argv = ActionSpec {
            name: "w.b".into(),
            ..Default::default()
        };
        assert!(parse_params(&no_argv, &[], args(&["x"]), Path::new("/")).is_err());
        assert!(
            parse_params(&no_argv, &[], Vec::new(), Path::new("/"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn durations_take_units() {
        assert_eq!(parse_duration("250ms"), Ok(250));
        assert_eq!(parse_duration("30s"), Ok(30_000));
        assert_eq!(parse_duration("30"), Ok(30_000), "bare = seconds");
        assert_eq!(parse_duration("1h"), Ok(3_600_000));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("-1s").is_err());
    }

    #[test]
    fn a_diff_prints_as_a_unified_diff() {
        let d = DiffView {
            files: vec![
                FileDiff {
                    old_path: "src/a.rs".into(),
                    new_path: "src/a.rs".into(),
                    status: FileStatus::FileModified as i32,
                    hunks: vec![Hunk {
                        old_start: 1,
                        old_lines: 2,
                        new_start: 1,
                        new_lines: 2,
                        header: "fn main()".into(),
                        body: b" keep\n-old\n+new".to_vec(),
                    }],
                    ..Default::default()
                },
                FileDiff {
                    new_path: "new.txt".into(),
                    status: FileStatus::FileAdded as i32,
                    hunks: vec![Hunk {
                        old_start: 0,
                        old_lines: 0,
                        new_start: 1,
                        new_lines: 1,
                        body: b"+hello\n".to_vec(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                FileDiff {
                    old_path: "gone.bin".into(),
                    status: FileStatus::FileDeleted as i32,
                    binary: true,
                    ..Default::default()
                },
                FileDiff {
                    old_path: "big.rs".into(),
                    new_path: "big.rs".into(),
                    status: FileStatus::FileModified as i32,
                    hunks_omitted: true,
                    additions: 9000,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            render_diff(&d),
            "diff --git a/src/a.rs b/src/a.rs\n\
             --- a/src/a.rs\n\
             +++ b/src/a.rs\n\
             @@ -1,2 +1,2 @@ fn main()\n keep\n-old\n+new\n\
             diff --git a/new.txt b/new.txt\n\
             new file\n\
             --- /dev/null\n\
             +++ b/new.txt\n\
             @@ -0,0 +1 @@\n+hello\n\
             diff --git a/gone.bin b/gone.bin\n\
             deleted file\n\
             Binary files a/gone.bin and /dev/null differ\n\
             diff --git a/big.rs b/big.rs\n\
             # +9000 -0: hunks omitted; ask again with file=big.rs\n"
        );
    }

    #[test]
    fn a_table_prints_aligned() {
        let t = TableView {
            columns: args(&["PATH", "BRANCH"]),
            rows: vec![
                TableRow {
                    cells: args(&["/a/long-path", "main"]),
                    ..Default::default()
                },
                TableRow {
                    cells: args(&["/b", "feat/x"]),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            render_table(&t),
            "PATH          BRANCH\n/a/long-path  main\n/b            feat/x\n"
        );
    }

    #[test]
    fn views_and_refusals_print() {
        let text = View {
            kind: Some(view::Kind::Text(TextView {
                title: "t".into(),
                text: "hello".into(),
            })),
        };
        assert_eq!(render_view(&text), "hello\n");
        let r = ActionRefused {
            reason: "dirty".into(),
            retry: [("force".to_string(), val(Value::Bool(true)))].into(),
        };
        assert_eq!(retry_hint(&r), "force=true");
    }
}
