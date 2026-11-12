use std::path::Path;
use std::process::Command;

use fleet_perf::approval::{ApprovalOutcome, check};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn toml(baseline: u64, tol_wall: f64, extra: &str) -> String {
    format!(
        r#"[gate]
approvers = ["gintarasm"]
tolerance = {{ instr = 0.02, wall = {tol_wall} }}

[metrics.a]
suite = "wall"
unit = "ns"
budget = 1000
baseline = {baseline}
{extra}"#
    )
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// Repo with `main` holding `base_toml`; HEAD is then on branch `work`.
fn repo(base_toml: &str) -> TempDir {
    let d = TempDir::new().unwrap();
    let p = d.path();
    git(p, &["init", "-b", "main"]);
    git(p, &["config", "user.name", "Test"]);
    git(p, &["config", "user.email", "test@example.com"]);
    git(p, &["config", "commit.gpgsign", "false"]);
    write(p, "bench/baselines.toml", base_toml);
    write(p, "docs/perf-log.md", "# log\n");
    git(p, &["add", "."]);
    git(p, &["commit", "-m", "base"]);
    git(p, &["checkout", "-b", "work"]);
    d
}

fn commit(d: &TempDir, msg: &str) {
    git(d.path(), &["add", "."]);
    git(d.path(), &["commit", "-m", msg]);
}

fn run(d: &TempDir) -> ApprovalOutcome {
    check(d.path(), "main").unwrap()
}

fn is_rejected(o: &ApprovalOutcome) -> bool {
    matches!(o, ApprovalOutcome::Rejected(_))
}

#[test]
fn no_change_not_needed() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "other.txt", "x");
    commit(&d, "unrelated");
    assert_eq!(run(&d), ApprovalOutcome::NotNeeded);
}

#[test]
fn tighten_not_needed() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "bench/baselines.toml", &toml(90, 0.05, ""));
    commit(&d, "tighten");
    assert_eq!(run(&d), ApprovalOutcome::NotNeeded);
}

#[test]
fn first_recording_not_needed() {
    let d = repo(&toml(0, 0.05, ""));
    write(d.path(), "bench/baselines.toml", &toml(500, 0.05, ""));
    commit(&d, "record");
    assert_eq!(run(&d), ApprovalOutcome::NotNeeded);
}

#[test]
fn raised_baseline_without_trailer_rejected() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "bench/baselines.toml", &toml(200, 0.05, ""));
    write(d.path(), "docs/perf-log.md", "# log\nraised\n");
    commit(&d, "raise");
    let o = run(&d);
    assert!(is_rejected(&o), "{o:?}");
    let ApprovalOutcome::Rejected(msg) = o else {
        unreachable!()
    };
    assert!(
        msg.contains("Perf-Approved") && msg.contains("loosened: a"),
        "{msg}"
    );
}

#[test]
fn trailer_without_perf_log_rejected() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "bench/baselines.toml", &toml(200, 0.05, ""));
    commit(&d, "raise\n\nPerf-Approved: gintarasm");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn trailer_from_non_approver_rejected() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "bench/baselines.toml", &toml(200, 0.05, ""));
    write(d.path(), "docs/perf-log.md", "# log\nraised\n");
    commit(&d, "raise\n\nPerf-Approved: claude");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn trailer_and_log_approved() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "bench/baselines.toml", &toml(200, 0.05, ""));
    write(d.path(), "docs/perf-log.md", "# log\nraised\n");
    commit(&d, "raise\n\nPerf-Approved: gintarasm");
    assert_eq!(run(&d), ApprovalOutcome::Approved);
}

#[test]
fn removed_metric_needs_approval() {
    let extra = "\n[metrics.b]\nsuite = \"wall\"\nunit = \"ns\"\nbaseline = 5\n";
    let d = repo(&toml(100, 0.05, extra));
    write(d.path(), "bench/baselines.toml", &toml(100, 0.05, ""));
    commit(&d, "remove b");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn raised_tolerance_needs_approval() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "bench/baselines.toml", &toml(100, 0.10, ""));
    commit(&d, "loosen tolerance");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn self_added_approver_rejected() {
    let d = repo(&toml(100, 0.05, ""));
    let evil = toml(200, 0.05, "").replace("[\"gintarasm\"]", "[\"gintarasm\", \"claude\"]");
    write(d.path(), "bench/baselines.toml", &evil);
    write(d.path(), "docs/perf-log.md", "# log\nraised\n");
    commit(&d, "raise\n\nPerf-Approved: claude");
    assert!(is_rejected(&run(&d)));
}
