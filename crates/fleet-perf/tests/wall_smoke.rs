use std::path::Path;
use std::process::Command;

use fleet_perf::results::{Results, Suite};
use fleet_perf::wall;

/// `fleet-perf run` finds `fleetd`, `fake-agent` and `fleet-gui` next to
/// itself, so make sure they exist in the profile this test runs under.
fn build_siblings() {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args([
        "build",
        "-p",
        "fleetd",
        "-p",
        "fleet-perf",
        "-p",
        "fleet-gui",
    ]);
    cmd.arg("--bins");
    if !cfg!(debug_assertions) {
        cmd.arg("--release");
    }
    assert!(cmd.status().expect("cargo build").success());
}

/// Set to also run the GUI scenarios (macOS; opens windows, needs an
/// unlocked screen): `FLEET_PERF_GUI_SMOKE=1 cargo test -p fleet-perf`.
const GUI_SMOKE_ENV: &str = "FLEET_PERF_GUI_SMOKE";

#[test]
fn wall_suite_quick_emits_all_metrics() {
    build_siblings();
    let bin = Path::new(env!("CARGO_BIN_EXE_fleet-perf"));
    let out = tempfile::tempdir().unwrap();
    let out_file = out.path().join("wall.json");
    let mut cmd = Command::new(bin);
    cmd.args(["run", "--suite", "wall", "--quick", "--out"])
        .arg(&out_file);
    // The GUI scenarios open windows (and need an unlocked screen): opt-in.
    let gui = cfg!(target_os = "macos") && std::env::var_os(GUI_SMOKE_ENV).is_some();
    if !gui {
        cmd.env(wall::NO_GUI_ENV, "1");
    }
    let status = cmd.status().expect("run fleet-perf");
    assert!(status.success());

    let r = Results::from_json(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    assert_eq!(r.suite, Suite::Wall);
    let names: Vec<_> = r.metrics.keys().map(String::as_str).collect();
    let mut expected = vec![
        "daemon_flood50_cpu_ns_per_mib",
        "daemon_flood50_lagged_subscriptions",
        "daemon_flood50_peak_rss_bytes",
        "daemon_flood50_wall_ns",
        "fleetd_cold_start_ns",
        "fleetd_idle_rss_bytes",
        "flood50_attached4_lagged_subscriptions",
        "flood50_cpu_ns_per_mib",
        "flood50_peak_rss_bytes",
        "flood50_wall_ns",
        "input_echo_flood50_ns",
        "reattach_snapshot_ns",
    ];
    if gui {
        expected.extend(wall::GUI_METRIC_NAMES);
        expected.sort_unstable();
    }
    assert_eq!(names, expected);
    for (name, m) in &r.metrics {
        // Quick payloads are too small for fleetd to drop a subscriber.
        if name == "daemon_flood50_lagged_subscriptions"
            || name == "flood50_attached4_lagged_subscriptions"
        {
            assert!(m.value >= 0.0, "{name} = {}", m.value);
            continue;
        }
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
