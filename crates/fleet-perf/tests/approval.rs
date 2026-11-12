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

fn rejected_or_err(d: &TempDir) -> bool {
    match check(d.path(), "main") {
        Err(_) => true,
        Ok(o) => is_rejected(&o),
    }
}

#[test]
fn nan_baseline_not_bypass() {
    let d = repo(&toml(100, 0.05, ""));
    let t = toml(100, 0.05, "").replace("baseline = 100", "baseline = nan");
    write(d.path(), "bench/baselines.toml", &t);
    commit(&d, "nan");
    assert!(rejected_or_err(&d));
}

#[test]
fn nan_tolerance_not_bypass() {
    let d = repo(&toml(100, 0.05, ""));
    let t = toml(100, 0.05, "").replace("wall = 0.05", "wall = nan");
    write(d.path(), "bench/baselines.toml", &t);
    commit(&d, "nan tol");
    assert!(rejected_or_err(&d));
}

#[test]
fn zeroed_baseline_needs_approval() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "bench/baselines.toml", &toml(0, 0.05, ""));
    commit(&d, "zero");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn suite_change_needs_approval() {
    let d = repo(&toml(100, 0.05, ""));
    let t = toml(100, 0.05, "").replace("suite = \"wall\"", "suite = \"instr\"");
    write(d.path(), "bench/baselines.toml", &t);
    commit(&d, "suite");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn unit_change_needs_approval() {
    let d = repo(&toml(100, 0.05, ""));
    let t = toml(100, 0.05, "").replace("unit = \"ns\"", "unit = \"ms\"");
    write(d.path(), "bench/baselines.toml", &t);
    commit(&d, "unit");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn unresolvable_base_ref_is_error() {
    let d = repo(&toml(100, 0.05, ""));
    assert!(check(d.path(), "no-such-ref").is_err());
}

#[test]
fn unreadable_merge_base_file_is_error_not_empty() {
    // Corrupt baselines at the merge-base must not be treated as absent.
    let d = repo("this is = not [valid");
    write(d.path(), "bench/baselines.toml", &toml(200, 0.05, ""));
    commit(&d, "raise");
    assert!(check(d.path(), "main").is_err());
}

#[test]
fn absent_at_merge_base_means_nothing_loosened() {
    let d = TempDir::new().unwrap();
    let p = d.path();
    git(p, &["init", "-b", "main"]);
    git(p, &["config", "user.name", "Test"]);
    git(p, &["config", "user.email", "test@example.com"]);
    git(p, &["config", "commit.gpgsign", "false"]);
    write(p, "README", "x");
    git(p, &["add", "."]);
    git(p, &["commit", "-m", "base"]);
    git(p, &["checkout", "-b", "work"]);
    write(p, "bench/baselines.toml", &toml(500, 0.05, ""));
    commit(&d, "add baselines");
    assert_eq!(run(&d), ApprovalOutcome::NotNeeded);
}

#[test]
fn adding_per_metric_tolerance_needs_approval() {
    let d = repo(&toml(100, 0.05, ""));
    write(
        d.path(),
        "bench/baselines.toml",
        &toml(100, 0.05, "tolerance = 0.10\n"),
    );
    commit(&d, "per-metric tolerance");
    let o = run(&d);
    let ApprovalOutcome::Rejected(msg) = o else {
        panic!("{o:?}")
    };
    assert!(msg.contains("loosened: a"), "{msg}");
}

#[test]
fn raising_per_metric_tolerance_needs_approval() {
    let d = repo(&toml(100, 0.05, "tolerance = 0.10\n"));
    write(
        d.path(),
        "bench/baselines.toml",
        &toml(100, 0.05, "tolerance = 0.20\n"),
    );
    commit(&d, "raise per-metric tolerance");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn tightening_per_metric_tolerance_not_needed() {
    let d = repo(&toml(100, 0.05, ""));
    write(
        d.path(),
        "bench/baselines.toml",
        &toml(100, 0.05, "tolerance = 0.03\n"),
    );
    commit(&d, "tighter per-metric tolerance");
    assert_eq!(run(&d), ApprovalOutcome::NotNeeded);
}

#[test]
fn enabling_report_only_needs_approval() {
    let d = repo(&toml(100, 0.05, ""));
    write(
        d.path(),
        "bench/baselines.toml",
        &toml(100, 0.05, "report_only = true\n"),
    );
    commit(&d, "report-only");
    let o = run(&d);
    let ApprovalOutcome::Rejected(msg) = o else {
        panic!("{o:?}")
    };
    assert!(msg.contains("loosened: a"), "{msg}");
}

#[test]
fn disabling_report_only_not_needed() {
    let d = repo(&toml(100, 0.05, "report_only = true\n"));
    write(
        d.path(),
        "bench/baselines.toml",
        &toml(100, 0.05, "report_only = false\n"),
    );
    commit(&d, "gate it again");
    assert_eq!(run(&d), ApprovalOutcome::NotNeeded);
}

/// Repo whose `main` also holds the gate code files.
fn repo_with_gate_code() -> TempDir {
    let d = repo(&toml(100, 0.05, ""));
    git(d.path(), &["checkout", "-q", "main"]);
    write(d.path(), "crates/fleet-perf/src/baseline.rs", "// gate\n");
    write(d.path(), ".github/workflows/ci.yml", "name: ci\n");
    commit(&d, "gate code");
    git(d.path(), &["checkout", "-q", "work"]);
    git(d.path(), &["merge", "-q", "--ff-only", "main"]);
    d
}

#[test]
fn gate_code_change_without_trailer_rejected() {
    let d = repo_with_gate_code();
    write(
        d.path(),
        "crates/fleet-perf/src/baseline.rs",
        "// weakened\n",
    );
    write(d.path(), "docs/perf-log.md", "# log\nchanged gate\n");
    commit(&d, "edit comparator");
    let o = run(&d);
    let ApprovalOutcome::Rejected(msg) = o else {
        panic!("{o:?}")
    };
    assert!(msg.contains("crates/fleet-perf/src/baseline.rs"), "{msg}");
}

#[test]
fn workflow_change_without_trailer_rejected() {
    let d = repo_with_gate_code();
    write(
        d.path(),
        ".github/workflows/ci.yml",
        "name: ci\n# skip perf\n",
    );
    commit(&d, "edit ci");
    let o = run(&d);
    let ApprovalOutcome::Rejected(msg) = o else {
        panic!("{o:?}")
    };
    assert!(msg.contains(".github/workflows/ci.yml"), "{msg}");
}

#[test]
fn gate_code_rename_without_trailer_rejected() {
    let d = repo_with_gate_code();
    git(
        d.path(),
        &[
            "mv",
            "crates/fleet-perf/src/baseline.rs",
            "crates/fleet-perf/src/other.rs",
        ],
    );
    commit(&d, "move comparator away");
    assert!(is_rejected(&run(&d)));
}

#[test]
fn gate_code_change_with_trailer_and_log_approved() {
    let d = repo_with_gate_code();
    write(
        d.path(),
        "crates/fleet-perf/src/baseline.rs",
        "// changed\n",
    );
    write(d.path(), "docs/perf-log.md", "# log\nchanged gate\n");
    commit(&d, "edit comparator\n\nPerf-Approved: gintarasm");
    assert_eq!(run(&d), ApprovalOutcome::Approved);
}

#[test]
fn gate_code_created_not_needed() {
    let d = repo(&toml(100, 0.05, ""));
    write(d.path(), "crates/fleet-perf/src/approval.rs", "// new\n");
    write(d.path(), ".github/workflows/ci.yml", "name: ci\n");
    commit(&d, "create gate code");
    assert_eq!(run(&d), ApprovalOutcome::NotNeeded);
}
