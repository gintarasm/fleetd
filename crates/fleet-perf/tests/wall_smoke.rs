use std::path::Path;
use std::process::Command;

use fleet_perf::results::{Results, Suite};
use fleet_perf::wall;

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

#[test]
fn flood_host_cpu_excludes_agent_cpu() {
    build_siblings();
    let exe = Path::new(env!("CARGO_BIN_EXE_fleet-perf"));
    let fake_agent = Path::new(env!("CARGO_BIN_EXE_fake-agent"));
    let run = wall::run_flood_host(exe, fake_agent, 8, 4 * 1024 * 1024).unwrap();
    assert!(run.host.cpu_ns > 0 && run.host.maxrss_bytes > 0);
    // wait4 charges the host for its reaped agents; the in-host figure must
    // not, so it has to come out strictly smaller.
    assert!(
        run.host.cpu_ns < run.wait4_cpu_ns,
        "host {} ns vs wait4 {} ns",
        run.host.cpu_ns,
        run.wait4_cpu_ns
    );
}
