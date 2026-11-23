//! `fleet search` (ADR 0098): every agent's output searched for a text,
//! printed as `ID:LINE:TEXT` (grep's shape) or JSON. Exit code 1 when
//! nothing matched, as grep's.

use std::io::{self, Write};

use clap::Args as ClapArgs;
use fleet_client::search::search;
use fleet_client::{Connection, Result};
use fleet_proto::{SearchHit, SearchHits, SearchOutput};
use serde_json::json;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The text to look for (plain text, not a pattern)
    pub text: String,
    /// Match case (default: case folded)
    #[arg(long)]
    pub case_sensitive: bool,
    /// At most N hits in all (default 200, at most 1000)
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
    /// At most N hits per agent (default 20)
    #[arg(long, value_name = "N")]
    pub per_agent: Option<u32>,
    /// Print a JSON object (`hits`, `truncated`)
    #[arg(long)]
    pub json: bool,
}

pub fn run(client: &mut Connection, args: &Args) -> Result<u8> {
    let hits = search(client, request(args))?;
    let mut out = io::stdout().lock();
    if args.json {
        writeln!(out, "{}", to_json(&hits))?;
    } else {
        for h in &hits.hits {
            if writeln!(out, "{}", line(h)).is_err() {
                // stdout closed (piped into `head`).
                return Ok(0);
            }
        }
        if hits.truncated {
            eprintln!(
                "fleet: more hits not shown (raise --limit or --per-agent, or narrow the text)"
            );
        }
    }
    out.flush()?;
    Ok(if hits.hits.is_empty() { 1 } else { 0 })
}

fn request(args: &Args) -> SearchOutput {
    SearchOutput {
        text: args.text.clone(),
        case_sensitive: args.case_sensitive,
        limit: args.limit.unwrap_or(0),
        per_agent: args.per_agent.unwrap_or(0),
    }
}

/// `ID:LINE:TEXT`.
fn line(h: &SearchHit) -> String {
    format!("{}:{}:{}", h.agent_id, h.line, h.text)
}

fn to_json(hits: &SearchHits) -> serde_json::Value {
    json!({
        "hits": hits.hits.iter().map(|h| json!({
            "agent_id": h.agent_id,
            "line": h.line,
            "text": h.text,
        })).collect::<Vec<_>>(),
        "truncated": hits.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(agent_id: u64, line: u64, text: &str) -> SearchHit {
        SearchHit {
            agent_id,
            line,
            text: text.into(),
        }
    }

    #[test]
    fn prints_grep_shaped_lines() {
        assert_eq!(
            line(&hit(7, 1234, "error: no such file")),
            "7:1234:error: no such file"
        );
    }

    #[test]
    fn json_has_every_hit_and_truncated() {
        let hits = SearchHits {
            hits: vec![hit(2, 5, "x")],
            truncated: true,
        };
        assert_eq!(
            to_json(&hits),
            json!({"hits": [{"agent_id": 2, "line": 5, "text": "x"}], "truncated": true})
        );
    }

    #[test]
    fn defaults_leave_the_caps_to_the_node() {
        let args = Args {
            text: "x".into(),
            case_sensitive: false,
            limit: None,
            per_agent: Some(3),
            json: false,
        };
        let req = request(&args);
        assert_eq!((req.limit, req.per_agent), (0, 3));
    }
}
