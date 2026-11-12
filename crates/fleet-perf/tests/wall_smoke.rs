use std::path::Path;
use std::process::Command;

use fleet_perf::results::{Results, Suite};

/// `fleet-perf run` finds `fleetd` and `fake-agent` next to itself, so make
/// sure both exist in the profile this test runs under.
fn build_siblings() {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["build", "-p", "fleetd", "-p", "fleet-perf", "--bins"]);
    if !cfg!(debug_assertions) {
        cmd.arg("--release");
    }
    assert!(cmd.status().expect("cargo build").success());
}

#[test]
fn wall_suite_quick_emits_all_metrics() {
    build_siblings();
    let bin = Path::new(env!("CARGO_BIN_EXE_fleet-perf"));
    let out = tempfile::tempdir().unwrap();
    let out_file = out.path().join("wall.json");
    let status = Command::new(bin)
        .args(["run", "--suite", "wall", "--quick", "--out"])
        .arg(&out_file)
        .status()
        .expect("run fleet-perf");
    assert!(status.success());

    let r = Results::from_json(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    assert_eq!(r.suite, Suite::Wall);
    let names: Vec<_> = r.metrics.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "fleetd_cold_start_ns",
            "fleetd_idle_rss_bytes",
            "flood50_cpu_ns_per_mib",
            "flood50_peak_rss_bytes",
            "flood50_wall_ns",
        ]
    );
    for (name, m) in &r.metrics {
        assert!(m.value > 0.0, "{name} = {}", m.value);
    }
}
