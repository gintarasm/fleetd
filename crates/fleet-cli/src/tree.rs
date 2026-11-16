//! `fleet tree` (ADR 0074, spec §8): the spawned-by tree, from the
//! `fleet.parent` labels, in box-drawing characters or JSON.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use clap::Args as ClapArgs;
use fleet_client::names::name_of;
use fleet_client::tree::{Tree, Visit};
use fleet_client::{Connection, Error, Result};
use fleet_proto::status::live_report;
use fleet_proto::{AgentInfo, AgentState, ListAgents, request, response};
use serde_json::{Value, json};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Print a JSON array of root nodes (each with `children`)
    #[arg(long)]
    pub json: bool,
    /// Include ended agents (default: only those with running descendants)
    #[arg(long)]
    pub all: bool,
    /// Only agents with this label k=v (repeatable); their ancestors stay
    #[arg(long = "label", value_name = "K=V")]
    pub labels: Vec<String>,
}

pub fn run(client: &mut Connection, args: &Args, selector: &HashMap<String, String>) -> Result<()> {
    let agents = match client.call(request::Kind::List(ListAgents::default()))? {
        response::Kind::Agents(list) => list.agents,
        _ => return Err(Error::Protocol("unexpected reply to list".into())),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let shown = select(&agents, args.all, selector);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_tree(&shown, now_ms)).unwrap_or_default()
        );
    } else {
        for line in text_lines(&shown, now_ms) {
            println!("{line}");
        }
    }
    Ok(())
}

fn ended(info: &AgentInfo) -> bool {
    matches!(info.state(), AgentState::Exited | AgentState::Lost)
}

/// The agents to draw, as their own tree: those matching `selector` that
/// are running (or, without `all`, ended with a running descendant), and
/// every ancestor of one. Ordered by id, so siblings are too.
pub fn select(agents: &[AgentInfo], all: bool, selector: &HashMap<String, String>) -> Selection {
    let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
    sorted.sort_by_key(|a| a.id);
    let full = Tree::of(sorted.iter().copied());
    let by_id: HashMap<u64, &AgentInfo> = sorted.iter().map(|a| (a.id, *a)).collect();
    let mut keep: HashSet<u64> = HashSet::new();
    for a in &sorted {
        let matches = selector.iter().all(|(k, v)| a.labels.get(k) == Some(v));
        let wanted = all || !ended(a) || full.descendants(a.id).iter().any(|d| !ended(by_id[d]));
        if matches && wanted {
            keep.insert(a.id);
            keep.extend(full.ancestors(a.id));
        }
    }
    let infos: Vec<AgentInfo> = sorted
        .into_iter()
        .filter(|a| keep.contains(&a.id))
        .cloned()
        .collect();
    let tree = Tree::of(&infos);
    Selection { infos, tree }
}

pub struct Selection {
    infos: Vec<AgentInfo>,
    tree: Tree,
}

impl Selection {
    fn info(&self, id: u64) -> &AgentInfo {
        self.infos.iter().find(|a| a.id == id).expect("in tree")
    }
}

/// The status a row shows. Ended agents as the GUI infers them (ADR 0042:
/// exit 0 is done, anything else failed), a lost one as such. The GUI also
/// uses the agent's last output to tell working, needs-you and idle; a
/// one-shot client gets no activity feed, so a running agent is `running`
/// unless it reported one (ADR 0075: that wins, while it runs).
pub fn status(info: &AgentInfo) -> &'static str {
    if let Some(r) = live_report(info) {
        return r.state().name();
    }
    let shell = info.labels.get("kind").is_some_and(|k| k == "shell");
    match info.state() {
        AgentState::Running if shell => "shell",
        AgentState::Running => "running",
        AgentState::Lost => "lost",
        AgentState::Exited if shell => "exited",
        AgentState::Exited => match info.exit_code {
            Some(0) => "done",
            _ => "failed",
        },
        AgentState::Unspecified => "unknown",
    }
}

/// How long it ran (so far, while running): `None` for a lost agent or an
/// exit without a recorded time.
fn run_secs(info: &AgentInfo, now_ms: i64) -> Option<u64> {
    let end = match info.state() {
        AgentState::Running => now_ms,
        AgentState::Exited => info.exited_unix_ms?,
        _ => return None,
    };
    u64::try_from(end.checked_sub(info.created_unix_ms)?)
        .ok()
        .map(|ms| ms / 1000)
}

/// "42s", "3m 12s", "2h 5m", "3d 4h".
pub fn format_run_time(s: u64) -> String {
    let (m, h, d) = (s / 60, s / 3600, s / 86_400);
    if d > 0 {
        format!("{d}d {}h", h % 24)
    } else if h > 0 {
        format!("{h}h {}m", m % 60)
    } else if m > 0 {
        format!("{m}m {}s", s % 60)
    } else {
        format!("{s}s")
    }
}

fn label(info: &AgentInfo) -> String {
    name_of(info).map_or_else(
        || {
            Path::new(&info.program).file_name().map_or_else(
                || info.program.clone(),
                |n| n.to_string_lossy().into_owned(),
            )
        },
        str::to_owned,
    )
}

fn program_name(info: &AgentInfo) -> String {
    Path::new(&info.program).file_name().map_or_else(
        || info.program.clone(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The box-drawing prefix of a walk line.
fn prefix(v: &Visit) -> String {
    let mut out = String::new();
    // The root's own flag draws nothing.
    let n = v.last.len();
    for last in &v.last[1..n.saturating_sub(1).max(1)] {
        out.push_str(if *last { "   " } else { "│  " });
    }
    if n > 1 {
        out.push_str(if v.last[n - 1] { "└─ " } else { "├─ " });
    }
    out
}

pub fn text_lines(sel: &Selection, now_ms: i64) -> Vec<String> {
    sel.tree
        .walk(|_| true)
        .iter()
        .map(|v| {
            let info = sel.info(v.id);
            let mut line = format!("{}{} {}", prefix(v), info.id, label(info));
            if name_of(info).is_some() {
                line.push_str(&format!("  {}", program_name(info)));
            }
            line.push_str(&format!("  {}", status(info)));
            if let Some(s) = run_secs(info, now_ms) {
                line.push_str(&format!("  {}", format_run_time(s)));
            }
            if let Some(r) = live_report(info).filter(|r| !r.message.is_empty()) {
                line.push_str(&format!("  \"{}\"", r.message));
            }
            line
        })
        .collect()
}

pub fn json_tree(sel: &Selection, now_ms: i64) -> Value {
    fn node(sel: &Selection, id: u64, now_ms: i64) -> Value {
        let info = sel.info(id);
        json!({
            "id": info.id,
            "name": name_of(info),
            "program": info.program,
            "state": status_state(info),
            "status": status(info),
            "reported": live_report(info).map(|r| json!({
                "state": r.state().name(),
                "message": r.message,
                "at_unix_ms": r.at_unix_ms,
            })),
            "exit_code": info.exit_code,
            "run_secs": run_secs(info, now_ms),
            "created_unix_ms": info.created_unix_ms,
            "labels": info.labels.iter().collect::<BTreeMap<_, _>>(),
            "children": sel
                .tree
                .children(id)
                .iter()
                .map(|&c| node(sel, c, now_ms))
                .collect::<Vec<_>>(),
        })
    }
    Value::Array(
        sel.tree
            .roots()
            .iter()
            .map(|&r| node(sel, r, now_ms))
            .collect(),
    )
}

fn status_state(info: &AgentInfo) -> &'static str {
    match info.state() {
        AgentState::Running => "running",
        AgentState::Exited => "exited",
        AgentState::Lost => "lost",
        AgentState::Unspecified => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: u64, parent: Option<u64>, state: AgentState, code: Option<i32>) -> AgentInfo {
        let mut a = AgentInfo {
            id,
            program: "/usr/bin/claude".into(),
            created_unix_ms: 0,
            exit_code: code,
            exited_unix_ms: (state == AgentState::Exited).then_some(65_000),
            ..Default::default()
        };
        a.set_state(state);
        if let Some(p) = parent {
            a.labels.insert("fleet.parent".into(), p.to_string());
        }
        a
    }

    fn named(mut a: AgentInfo, name: &str) -> AgentInfo {
        a.labels.insert("name".into(), name.into());
        a
    }

    const NONE: Option<i32> = None;

    #[test]
    fn draws_boxes_in_id_order() {
        use AgentState::*;
        let agents = [
            agent(5, None, Running, NONE),
            named(agent(2, Some(1), Running, NONE), "reviewer"),
            agent(1, None, Running, NONE),
            agent(3, Some(1), Running, NONE),
            agent(4, Some(2), Running, NONE),
        ];
        let sel = select(&agents, false, &HashMap::new());
        let lines = text_lines(&sel, 10_000);
        assert_eq!(
            lines,
            [
                "1 claude  running  10s",
                "├─ 2 reviewer  claude  running  10s",
                "│  └─ 4 claude  running  10s",
                "└─ 3 claude  running  10s",
                "5 claude  running  10s",
            ]
        );
    }

    #[test]
    fn ended_agents_show_only_with_running_descendants_unless_all() {
        use AgentState::*;
        let agents = [
            agent(1, None, Exited, Some(0)),
            agent(2, Some(1), Running, NONE),
            agent(3, None, Exited, Some(1)),
            agent(4, Some(3), Exited, Some(0)),
            agent(5, None, Lost, NONE),
        ];
        let ids = |all| -> Vec<String> {
            text_lines(&select(&agents, all, &HashMap::new()), 70_000)
                .into_iter()
                .map(|l| l.trim_start_matches(['│', ' ', '└', '├', '─']).to_owned())
                .collect()
        };
        assert_eq!(
            ids(false),
            ["1 claude  done  1m 5s", "2 claude  running  1m 10s"]
        );
        let all = ids(true);
        assert_eq!(all.len(), 5);
        assert_eq!(all[2], "3 claude  failed  1m 5s");
        assert_eq!(all[4], "5 claude  lost");
    }

    #[test]
    fn label_filter_keeps_ancestors() {
        use AgentState::*;
        let mut leaf = agent(3, Some(2), Running, NONE);
        leaf.labels.insert("role".into(), "reviewer".into());
        let agents = [
            agent(1, None, Running, NONE),
            agent(2, Some(1), Running, NONE),
            leaf,
            agent(4, Some(1), Running, NONE),
        ];
        let sel = select(
            &agents,
            false,
            &HashMap::from([("role".to_owned(), "reviewer".to_owned())]),
        );
        let ids: Vec<u64> = sel.tree.walk(|_| true).iter().map(|v| v.id).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn orphans_and_loops_are_roots() {
        use AgentState::*;
        let agents = [
            agent(2, Some(99), Running, NONE),
            agent(3, Some(4), Running, NONE),
            agent(4, Some(3), Running, NONE),
        ];
        let sel = select(&agents, false, &HashMap::new());
        assert_eq!(text_lines(&sel, 0).len(), 3);
        assert_eq!(sel.tree.roots(), [2, 3, 4]);
    }

    #[test]
    fn a_shell_is_a_shell() {
        let mut a = agent(1, None, AgentState::Running, NONE);
        a.labels.insert("kind".into(), "shell".into());
        assert_eq!(status(&a), "shell");
    }

    #[test]
    fn json_nests_children() {
        use AgentState::*;
        let agents = [
            agent(1, None, Running, NONE),
            agent(2, Some(1), Exited, Some(0)),
        ];
        let v = json_tree(&select(&agents, true, &HashMap::new()), 5_000);
        assert_eq!(v[0]["id"], 1);
        assert_eq!(v[0]["status"], "running");
        assert_eq!(v[0]["children"][0]["id"], 2);
        assert_eq!(v[0]["children"][0]["status"], "done");
        assert_eq!(v[0]["children"][0]["run_secs"], 65);
        assert_eq!(v[0]["children"][0]["children"], json!([]));
    }

    #[test]
    fn run_times_read_naturally() {
        assert_eq!(format_run_time(0), "0s");
        assert_eq!(format_run_time(192), "3m 12s");
        assert_eq!(format_run_time(7_500), "2h 5m");
        assert_eq!(format_run_time(100_000), "1d 3h");
    }
}
