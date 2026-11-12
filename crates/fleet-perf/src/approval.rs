//! Approval check: a loosened perf baseline is only accepted with an explicit
//! `Perf-Approved: <approver>` trailer and a `docs/perf-log.md` entry.
//!
//! The approvers list is read from the merge-base, never from HEAD, so a
//! branch cannot add its own approver.

use std::fmt;
use std::path::Path;
use std::process::Command;

use crate::baseline::Baselines;

const BASELINES_PATH: &str = "bench/baselines.toml";
const PERF_LOG_PATH: &str = "docs/perf-log.md";
const DEFAULT_APPROVER: &str = "gintarasm";

#[derive(Debug)]
pub enum Error {
    Git(String),
    Parse(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Git(m) | Error::Parse(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    NotNeeded,
    Approved,
    Rejected(String),
}

/// Names of metrics (or gate settings) that got looser from `old` to `new`.
pub fn loosened(old: &Baselines, new: &Baselines) -> Vec<String> {
    let mut out = Vec::new();
    for (name, o) in &old.metrics {
        let Some(n) = new.metrics.get(name) else {
            out.push(name.clone());
            continue;
        };
        let baseline_raised = o.baseline != 0.0 && n.baseline > o.baseline;
        let budget_loosened = match (o.budget, n.budget) {
            (Some(_), None) => true,
            (Some(ob), Some(nb)) => nb > ob,
            _ => false,
        };
        if baseline_raised || budget_loosened {
            out.push(name.clone());
        }
    }
    let (ot, nt) = (old.gate.tolerance, new.gate.tolerance);
    if nt.instr > ot.instr {
        out.push("gate.tolerance.instr".into());
    }
    if nt.wall > ot.wall {
        out.push("gate.tolerance.wall".into());
    }
    if old.gate.approvers != new.gate.approvers {
        out.push("gate.approvers".into());
    }
    out
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output, Error> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| Error::Git(format!("running git: {e}")))
}

fn git_ok(repo: &Path, args: &[&str]) -> Result<String, Error> {
    let out = git(repo, args)?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Contents of `bench/baselines.toml` at `rev`, or None when absent there.
fn baselines_at(repo: &Path, rev: &str) -> Result<Option<Baselines>, Error> {
    let spec = format!("{rev}:{BASELINES_PATH}");
    let out = git(repo, &["show", &spec])?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Baselines::parse(&text)
        .map(Some)
        .map_err(|e| Error::Parse(format!("parsing {BASELINES_PATH} at {rev}: {e}")))
}

pub fn check(repo: &Path, base: &str) -> Result<ApprovalOutcome, Error> {
    let mb = git_ok(repo, &["merge-base", base, "HEAD"])?
        .trim()
        .to_string();
    let new = baselines_at(repo, "HEAD")?
        .ok_or_else(|| Error::Parse(format!("{BASELINES_PATH} is missing at HEAD")))?;
    let old = match baselines_at(repo, &mb)? {
        Some(o) => o,
        None => Baselines {
            gate: crate::baseline::Gate {
                approvers: vec![DEFAULT_APPROVER.to_string()],
                tolerance: new.gate.tolerance,
            },
            metrics: Default::default(),
        },
    };

    let loose = loosened(&old, &new);
    if loose.is_empty() {
        return Ok(ApprovalOutcome::NotNeeded);
    }

    let range = format!("{mb}..HEAD");
    let trailers = git_ok(
        repo,
        &[
            "log",
            "--format=%(trailers:key=Perf-Approved,valueonly)",
            &range,
        ],
    )?;
    let approved = trailers
        .lines()
        .map(str::trim)
        .any(|v| old.gate.approvers.iter().any(|a| a == v));
    let changed = git_ok(repo, &["diff", "--name-only", &mb, "HEAD"])?;
    let logged = changed.lines().any(|l| l.trim() == PERF_LOG_PATH);

    let mut problems = Vec::new();
    if !approved {
        problems.push(format!(
            "no commit in {range} has a `Perf-Approved: <name>` trailer from an approver ({})",
            old.gate.approvers.join(", ")
        ));
    }
    if !logged {
        problems.push(format!("{PERF_LOG_PATH} was not changed in {range}"));
    }
    if problems.is_empty() {
        return Ok(ApprovalOutcome::Approved);
    }
    Ok(ApprovalOutcome::Rejected(format!(
        "loosened perf gate needs user approval; loosened: {}; {}",
        loose.join(", "),
        problems.join("; ")
    )))
}
