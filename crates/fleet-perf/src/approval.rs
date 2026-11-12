//! Approval check: a loosened perf baseline is only accepted with an explicit
//! `Perf-Approved: <approver>` trailer and a `docs/perf-log.md` entry.
//!
//! The approvers list is read from the merge-base, never from HEAD, so a
//! branch cannot add its own approver.
//!
//! Changing the gate's own code (comparator, approval check, suites, CI
//! workflows) needs the same approval, so the gate cannot be weakened
//! instead of the baselines. Files created on the branch are exempt.

use std::fmt;
use std::path::Path;
use std::process::Command;

use crate::baseline::Baselines;

const BASELINES_PATH: &str = "bench/baselines.toml";
const PERF_LOG_PATH: &str = "docs/perf-log.md";
const DEFAULT_APPROVER: &str = "gintarasm";
/// Gate code: changing (or deleting) any of these needs approval when it
/// existed at the merge-base. Entries ending in `/` are directory prefixes.
const GATE_PATHS: &[&str] = &[
    "crates/fleet-perf/src/baseline.rs",
    "crates/fleet-perf/src/approval.rs",
    "crates/fleet-perf/src/wall.rs",
    "crates/fleet-perf/src/instr.rs",
    "crates/fleet-perf/benches/instr.rs",
    ".github/workflows/",
];

fn is_gate_path(path: &str) -> bool {
    GATE_PATHS.iter().any(|g| match g.strip_suffix('/') {
        Some(dir) => path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/')),
        None => path == *g,
    })
}

fn exists_at(repo: &Path, rev: &str, path: &str) -> Result<bool, Error> {
    let tree = git_ok(repo, &["ls-tree", "--name-only", rev, "--", path])?;
    Ok(!tree.trim().is_empty())
}

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
        let baseline_raised = o.baseline != 0.0 && (n.baseline > o.baseline || n.baseline == 0.0);
        if o.suite != n.suite || o.unit != n.unit {
            out.push(name.clone());
            continue;
        }
        let budget_loosened = match (o.budget, n.budget) {
            (Some(_), None) => true,
            (Some(ob), Some(nb)) => nb > ob,
            _ => false,
        };
        // Relative to the effective tolerance at the merge-base, so adding an
        // override above the suite tolerance counts as well as raising one.
        let tolerance_loosened = new.gate.tolerance_for(n) > old.gate.tolerance_for(o);
        let made_report_only = n.report_only && !o.report_only;
        if baseline_raised || budget_loosened || tolerance_loosened || made_report_only {
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
    let commit = format!("{rev}^{{commit}}");
    git_ok(repo, &["rev-parse", "--verify", "--quiet", &commit])?;
    let spec = format!("{rev}:{BASELINES_PATH}");
    // Only a genuinely absent path maps to None; any other failure is an error.
    if !git(repo, &["cat-file", "-e", &spec])?.status.success() {
        let tree = git_ok(repo, &["ls-tree", "--name-only", rev, BASELINES_PATH])?;
        if tree.trim().is_empty() {
            return Ok(None);
        }
    }
    let out = git(repo, &["show", &spec])?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "git show {spec} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
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
    // --no-renames: a rename must show the old (gate) path as deleted.
    let changed = git_ok(repo, &["diff", "--no-renames", "--name-only", &mb, "HEAD"])?;
    let mut gate_changed = Vec::new();
    for path in changed.lines().map(str::trim).filter(|l| is_gate_path(l)) {
        if exists_at(repo, &mb, path)? {
            gate_changed.push(path.to_string());
        }
    }
    if loose.is_empty() && gate_changed.is_empty() {
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
    let mut what = Vec::new();
    if !loose.is_empty() {
        what.push(format!("loosened: {}", loose.join(", ")));
    }
    if !gate_changed.is_empty() {
        what.push(format!("changed gate files: {}", gate_changed.join(", ")));
    }
    Ok(ApprovalOutcome::Rejected(format!(
        "loosened perf gate needs user approval; {}; {}",
        what.join("; "),
        problems.join("; ")
    )))
}
