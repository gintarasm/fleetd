//! `fleet pending`, `fleet approve`, `fleet deny` (ADR 0079): the user's
//! answer to agents' spawns waiting for approval (`[node] spawn_approval =
//! true`). Exit codes (spec §1): 0 ok, 1 no such pending spawn, 2 usage or
//! other error, 3 forbidden (run inside an agent: only the user approves).

use std::collections::HashMap;

use clap::Args as ClapArgs;
use fleet_client::names::NAME_LABEL;
use fleet_client::{Connection, Error, Result};
use fleet_proto::{DecideSpawn, ListAgents, ListPendingSpawns, PendingSpawn, request, response};
use serde_json::{Value, json};

#[derive(ClapArgs, Debug)]
pub struct ListArgs {
    /// Print a JSON array instead of a table
    #[arg(long)]
    pub json: bool,
}

#[derive(ClapArgs, Debug)]
pub struct DecideArgs {
    /// The pending spawn's id (from `fleet pending`)
    pub id: u64,
}

/// Lists the spawns waiting for approval; the exit code.
pub fn list(client: &mut Connection, args: &ListArgs) -> Result<u8> {
    let pending = match client.call(request::Kind::ListPendingSpawns(ListPendingSpawns {}))? {
        response::Kind::PendingSpawns(p) => p.pending,
        _ => return Err(Error::Protocol("unexpected reply to pending".into())),
    };
    if args.json {
        let items: Vec<Value> = pending.iter().map(json_pending).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&items).unwrap_or_default()
        );
        return Ok(0);
    }
    if pending.is_empty() {
        println!("no spawns are waiting for approval");
        return Ok(0);
    }
    let names = match client.call(request::Kind::List(ListAgents::default()))? {
        response::Kind::Agents(list) => list
            .agents
            .into_iter()
            .filter_map(|a| Some((a.id, a.labels.get(NAME_LABEL)?.clone())))
            .collect(),
        _ => HashMap::new(),
    };
    for line in table(&pending, &names, now_ms()) {
        println!("{line}");
    }
    Ok(0)
}

/// Approves or denies pending spawn `args.id`; the exit code.
pub fn decide(client: &mut Connection, args: &DecideArgs, approve: bool) -> Result<u8> {
    let req = DecideSpawn {
        id: args.id,
        approve,
    };
    match client.call(request::Kind::DecideSpawn(req)) {
        Ok(_) => Ok(0),
        // Exit 3, from `main`.
        Err(e) if e.is_forbidden() => Err(e),
        Err(Error::Server(m)) if m.starts_with("no pending spawn") => {
            eprintln!("fleet: {m}");
            Ok(1)
        }
        Err(e) => {
            eprintln!("fleet: {e}");
            Ok(2)
        }
    }
}

fn json_pending(p: &PendingSpawn) -> Value {
    let labels: serde_json::Map<String, Value> = p
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    json!({
        "id": p.id,
        "parent": p.parent,
        "program": p.program,
        "args": p.args,
        "cwd": p.cwd,
        "labels": labels,
        "asked_unix_ms": p.asked_unix_ms,
        "expires_unix_ms": p.expires_unix_ms,
    })
}

/// One block per pending spawn: id, who asks, the command, time left; then
/// its cwd and labels (`fleet.*` left out: the parent is already named).
fn table(pending: &[PendingSpawn], names: &HashMap<u64, String>, now_ms: i64) -> Vec<String> {
    let mut out = Vec::new();
    for p in pending {
        let from = match names.get(&p.parent) {
            Some(n) => format!("agent {} ({n})", p.parent),
            None => format!("agent {}", p.parent),
        };
        let left = (p.expires_unix_ms - now_ms).max(0) / 1000;
        let command = std::iter::once(p.program.as_str())
            .chain(p.args.iter().map(String::as_str))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ");
        out.push(format!(
            "{}  {from} asks to run: {command}  ({}m{:02}s left)",
            p.id,
            left / 60,
            left % 60
        ));
        out.push(format!("    in {}", p.cwd));
        let mut labels: Vec<String> = p
            .labels
            .iter()
            .filter(|(k, _)| !k.starts_with("fleet."))
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if !labels.is_empty() {
            labels.sort();
            out.push(format!("    labels {}", labels.join(" ")));
        }
    }
    out
}

/// `arg` as a shell would need it, roughly: quoted when it has a space or
/// a quote, or is empty.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\'', '"', '\t', '\n']) {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_shows_who_asks_what_where_and_for_how_long() {
        let p = PendingSpawn {
            id: 3,
            parent: 7,
            program: "claude".into(),
            args: vec!["-p".into(), "fix the bug".into()],
            cwd: "/src/app".into(),
            labels: [("team", "x"), ("fleet.parent", "7"), ("name", "fixer")]
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            asked_unix_ms: 0,
            expires_unix_ms: 300_000,
        };
        let names = HashMap::from([(7, "lead".to_owned())]);
        assert_eq!(
            table(&[p], &names, 30_500),
            vec![
                "3  agent 7 (lead) asks to run: claude -p 'fix the bug'  (4m29s left)",
                "    in /src/app",
                "    labels name=fixer team=x",
            ]
        );
    }

    #[test]
    fn json_lists_every_field() {
        let p = PendingSpawn {
            id: 1,
            parent: 2,
            program: "sh".into(),
            ..Default::default()
        };
        let v = json_pending(&p);
        assert_eq!(v["id"], 1);
        assert_eq!(v["parent"], 2);
        assert_eq!(v["program"], "sh");
        assert!(v["labels"].as_object().unwrap().is_empty());
    }
}
