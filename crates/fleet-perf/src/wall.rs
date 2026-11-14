//! Wall-clock suite: fleetd cold start, idle RSS, the 50-agent flood, and the
//! daemon scenarios (flood through the protocol server, the flood with four
//! attached agents and input echo through it, reattach).

mod daemon;

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_node::pty::{AgentId, CommandSpec, PtyHost};
use serde::{Deserialize, Serialize};

use crate::results::{Measurement, Results, Suite};

pub const COLD_START_NS: &str = "fleetd_cold_start_ns";
pub const IDLE_RSS_BYTES: &str = "fleetd_idle_rss_bytes";
pub const FLOOD_WALL_NS: &str = "flood50_wall_ns";
pub const FLOOD_CPU_NS_PER_MIB: &str = "flood50_cpu_ns_per_mib";
pub const FLOOD_PEAK_RSS_BYTES: &str = "flood50_peak_rss_bytes";
pub const DAEMON_FLOOD_WALL_NS: &str = "daemon_flood50_wall_ns";
pub const DAEMON_FLOOD_CPU_NS_PER_MIB: &str = "daemon_flood50_cpu_ns_per_mib";
pub const DAEMON_FLOOD_PEAK_RSS_BYTES: &str = "daemon_flood50_peak_rss_bytes";
pub const DAEMON_FLOOD_LAGGED_SUBSCRIPTIONS: &str = "daemon_flood50_lagged_subscriptions";
pub const REATTACH_SNAPSHOT_NS: &str = "reattach_snapshot_ns";
pub const FLOOD_ATTACHED4_LAGGED_SUBSCRIPTIONS: &str = "flood50_attached4_lagged_subscriptions";
pub const INPUT_ECHO_FLOOD_NS: &str = "input_echo_flood50_ns";
/// Every metric the wall suite emits.
pub const METRIC_NAMES: &[&str] = &[
    COLD_START_NS,
    IDLE_RSS_BYTES,
    FLOOD_WALL_NS,
    FLOOD_CPU_NS_PER_MIB,
    FLOOD_PEAK_RSS_BYTES,
    DAEMON_FLOOD_WALL_NS,
    DAEMON_FLOOD_CPU_NS_PER_MIB,
    DAEMON_FLOOD_PEAK_RSS_BYTES,
    DAEMON_FLOOD_LAGGED_SUBSCRIPTIONS,
    REATTACH_SNAPSHOT_NS,
    FLOOD_ATTACHED4_LAGGED_SUBSCRIPTIONS,
    INPUT_ECHO_FLOOD_NS,
];

const FLOOD_AGENTS: usize = 50;
const FLOOD_BYTES: u64 = 8 * 1024 * 1024;
const QUICK_FLOOD_BYTES: u64 = 256 * 1024;
/// Attaches measured by `reattach_snapshot_ns` (median taken).
const REATTACHES: usize = 50;
const QUICK_REATTACHES: usize = 5;
/// Agents the client is attached to in the `flood50_attached4` scenarios (a
/// GUI showing four panes).
const FLOOD_ATTACHED: usize = 4;
/// Echo round trips measured by `input_echo_flood50_ns` (median taken), and
/// the pause between them.
const ECHO_SAMPLES: usize = 100;
const QUICK_ECHO_SAMPLES: usize = 10;
const ECHO_GAP: Duration = Duration::from_millis(5);
const MIB: f64 = 1024.0 * 1024.0;
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const TIGHT_POLL: Duration = Duration::from_millis(50);

/// A spawned child that is killed and reaped on drop.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sibling(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let path = exe.parent().ok_or("current_exe has no parent")?.join(name);
    if path.exists() {
        Ok(path)
    } else {
        Err(format!(
            "{} not found next to fleet-perf (build the workspace first)",
            path.display()
        ))
    }
}

fn spawn_fleetd(fleetd: &Path, state_dir: &Path) -> Result<Reaped, String> {
    Command::new(fleetd)
        .arg("--state-dir")
        .arg(state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Reaped)
        .map_err(|e| format!("spawning {}: {e}", fleetd.display()))
}

/// Poll until the daemon socket accepts a connection: tight (yielding) for the
/// first 50 ms so cold start is not quantised by sleeps, then every millisecond.
fn wait_ready(daemon: &mut Reaped, sock: &Path) -> Result<(), String> {
    let begin = Instant::now();
    let deadline = begin + READY_TIMEOUT;
    loop {
        if UnixStream::connect(sock).is_ok() {
            return Ok(());
        }
        if let Ok(Some(status)) = daemon.0.try_wait() {
            return Err(format!("fleetd exited before ready: {status}"));
        }
        if Instant::now() >= deadline {
            return Err("fleetd not ready within 10s".into());
        }
        if begin.elapsed() < TIGHT_POLL {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn cold_start_ns(fleetd: &Path) -> Result<f64, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut daemon = spawn_fleetd(fleetd, dir.path())?;
    wait_ready(&mut daemon, &dir.path().join("fleetd.sock"))?;
    Ok(start.elapsed().as_nanos() as f64)
}

fn rss_bytes(pid: u32) -> Result<f64, String> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm"))
            .map_err(|e| format!("reading statm: {e}"))?;
        let pages: f64 = statm
            .split_whitespace()
            .nth(1)
            .and_then(|f| f.parse().ok())
            .ok_or("malformed statm")?;
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Ok(pages * page as f64)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .map_err(|e| format!("running ps: {e}"))?;
        let kib: f64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .map_err(|_| "ps printed no RSS".to_string())?;
        Ok(kib * 1024.0)
    }
}

fn idle_rss_bytes(fleetd: &Path) -> Result<f64, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut daemon = spawn_fleetd(fleetd, dir.path())?;
    wait_ready(&mut daemon, &dir.path().join("fleetd.sock"))?;
    std::thread::sleep(Duration::from_secs(1));
    rss_bytes(daemon.0.id())
}

struct Flood {
    wall_ns: f64,
    cpu_ns_per_mib: f64,
    peak_rss_bytes: f64,
}

/// Process-wide usage of `flood-host` itself (PtyHost and its reader threads,
/// not the agents), printed by the host as one JSON line.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HostUsage {
    pub cpu_ns: u64,
    pub maxrss_bytes: u64,
}

/// Outcome of one `flood-host` child run.
#[derive(Debug, Clone, Copy)]
pub struct FloodRun {
    pub wall_ns: f64,
    pub host: HostUsage,
    /// `wait4` rusage of the host process, which the OS also charges for its
    /// reaped agent children. Kept to show the host figure excludes them.
    pub wait4_cpu_ns: u64,
}

fn tv_ns(tv: libc::timeval) -> u64 {
    tv.tv_sec as u64 * 1_000_000_000 + tv.tv_usec as u64 * 1_000
}

/// `ru_maxrss` is bytes on macOS and KiB elsewhere; normalise to bytes.
fn maxrss_bytes(ru: &libc::rusage) -> u64 {
    let v = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        v
    } else {
        v * 1024
    }
}

fn self_usage() -> HostUsage {
    // SAFETY: zeroed rusage is a valid out-parameter.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    HostUsage {
        cpu_ns: tv_ns(ru.ru_utime) + tv_ns(ru.ru_stime),
        maxrss_bytes: maxrss_bytes(&ru),
    }
}

/// Run `flood-host` as a child of this process. Wall time and exit status come
/// from the parent; CPU and peak RSS are the host's own `RUSAGE_SELF` report.
pub fn run_flood_host(
    exe: &Path,
    fake_agent: &Path,
    agents: usize,
    bytes: u64,
) -> Result<FloodRun, String> {
    let start = Instant::now();
    let mut child = Command::new(exe)
        .args(["flood-host", "--agents", &agents.to_string()])
        .args(["--bytes", &bytes.to_string(), "--fake-agent"])
        .arg(fake_agent)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning flood-host: {e}"))?;
    let mut out = String::new();
    let read = child
        .stdout
        .take()
        .ok_or("flood-host stdout not piped")?
        .read_to_string(&mut out);
    let pid = child.id() as libc::pid_t;
    // Reaped by wait4 below; forget the handle so it is not waited twice.
    std::mem::forget(child);

    let mut status = 0;
    // SAFETY: zeroed rusage is a valid out-parameter; pid is our child.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
    let wall_ns = start.elapsed().as_nanos() as f64;
    if rc != pid {
        return Err(format!("wait4: {}", std::io::Error::last_os_error()));
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Err(format!("flood-host failed (wait status {status})"));
    }
    read.map_err(|e| format!("reading flood-host output: {e}"))?;
    let host: HostUsage = serde_json::from_str(out.trim())
        .map_err(|e| format!("parsing flood-host output {out:?}: {e}"))?;
    Ok(FloodRun {
        wall_ns,
        host,
        wait4_cpu_ns: tv_ns(ru.ru_utime) + tv_ns(ru.ru_stime),
    })
}

fn flood(bytes: u64) -> Result<Flood, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let run = run_flood_host(&exe, &sibling("fake-agent")?, FLOOD_AGENTS, bytes)?;
    let total_mib = (FLOOD_AGENTS as u64 * bytes) as f64 / MIB;
    Ok(Flood {
        wall_ns: run.wall_ns,
        cpu_ns_per_mib: run.host.cpu_ns as f64 / total_mib,
        peak_rss_bytes: run.host.maxrss_bytes as f64,
    })
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

fn minimum(v: Vec<f64>) -> f64 {
    v.into_iter().fold(f64::INFINITY, f64::min)
}

fn repeat(
    n: usize,
    stat: fn(Vec<f64>) -> f64,
    mut f: impl FnMut() -> Result<f64, String>,
) -> Result<f64, String> {
    (0..n).map(|_| f()).collect::<Result<Vec<_>, _>>().map(stat)
}

pub fn run(quick: bool) -> Result<Results, String> {
    let fleetd = sibling("fleetd")?;
    let (cold_runs, rss_runs, flood_runs, bytes) = if quick {
        (1, 1, 1, QUICK_FLOOD_BYTES)
    } else {
        (20, 5, 5, FLOOD_BYTES)
    };

    // Minimum, not median: process-spawn latency on macOS has a heavy right
    // tail, and the minimum is the stable statistic for startup latency.
    let cold = repeat(cold_runs, minimum, || cold_start_ns(&fleetd))?;
    let rss = repeat(rss_runs, median, || idle_rss_bytes(&fleetd))?;
    let mut floods = Vec::new();
    for _ in 0..flood_runs {
        floods.push(flood(bytes)?);
    }
    let pick = |f: fn(&Flood) -> f64| median(floods.iter().map(f).collect());

    let fake_agent = sibling("fake-agent")?;
    let (daemon_runs, reattaches) = if quick {
        (1, QUICK_REATTACHES)
    } else {
        (3, REATTACHES)
    };
    let mut daemon_floods = Vec::new();
    for _ in 0..daemon_runs {
        daemon_floods.push(daemon::flood(&fleetd, &fake_agent, FLOOD_AGENTS, bytes)?);
    }
    let pick_daemon =
        |f: fn(&daemon::DaemonFlood) -> f64| median(daemon_floods.iter().map(f).collect());
    let reattach = daemon::reattach_snapshot_ns(&fleetd, &fake_agent, bytes, reattaches)?;

    // A client showing four panes must keep them streaming through the
    // flood: the worst run counts.
    let mut attached4_lagged = 0.0f64;
    for _ in 0..daemon_runs {
        let lagged = daemon::flood_attached_lagged(
            &fleetd,
            &fake_agent,
            FLOOD_AGENTS,
            FLOOD_ATTACHED,
            bytes,
        )?;
        attached4_lagged = attached4_lagged.max(lagged);
    }
    let echo_samples = if quick {
        QUICK_ECHO_SAMPLES
    } else {
        ECHO_SAMPLES
    };
    let echo = daemon::input_echo_ns(
        &fleetd,
        &fake_agent,
        FLOOD_AGENTS,
        FLOOD_ATTACHED,
        bytes,
        echo_samples,
        ECHO_GAP,
    )?;
    if echo.during_flood < echo_samples {
        // The quick flood is over almost at once; a full one must outlast
        // the samples or they do not measure the flood.
        let msg = format!(
            "input echo: only {} of {echo_samples} samples completed during the flood",
            echo.during_flood
        );
        if !quick {
            return Err(msg);
        }
        eprintln!("fleet-perf: {msg}");
    }

    let m = |value: f64, unit: &str| Measurement {
        value,
        unit: unit.to_string(),
    };
    let metrics = BTreeMap::from([
        (COLD_START_NS.to_string(), m(cold, "ns")),
        (IDLE_RSS_BYTES.to_string(), m(rss, "bytes")),
        (FLOOD_WALL_NS.to_string(), m(pick(|f| f.wall_ns), "ns")),
        (
            FLOOD_CPU_NS_PER_MIB.to_string(),
            m(pick(|f| f.cpu_ns_per_mib), "ns"),
        ),
        (
            FLOOD_PEAK_RSS_BYTES.to_string(),
            m(pick(|f| f.peak_rss_bytes), "bytes"),
        ),
        (
            DAEMON_FLOOD_WALL_NS.to_string(),
            m(pick_daemon(|f| f.wall_ns), "ns"),
        ),
        (
            DAEMON_FLOOD_CPU_NS_PER_MIB.to_string(),
            m(pick_daemon(|f| f.cpu_ns_per_mib), "ns"),
        ),
        (
            DAEMON_FLOOD_PEAK_RSS_BYTES.to_string(),
            m(pick_daemon(|f| f.peak_rss_bytes), "bytes"),
        ),
        (
            DAEMON_FLOOD_LAGGED_SUBSCRIPTIONS.to_string(),
            m(pick_daemon(|f| f.lagged_subscriptions), "count"),
        ),
        (REATTACH_SNAPSHOT_NS.to_string(), m(reattach, "ns")),
        (
            FLOOD_ATTACHED4_LAGGED_SUBSCRIPTIONS.to_string(),
            m(attached4_lagged, "count"),
        ),
        (INPUT_ECHO_FLOOD_NS.to_string(), m(echo.median_ns, "ns")),
    ]);
    Ok(Results {
        suite: Suite::Wall,
        metrics,
    })
}

/// Body of the hidden `flood-host` subcommand: run `agents` fake agents under
/// a `PtyHost`, check each read at least `bytes` and exited 0, and return this
/// process's own resource usage.
pub fn flood_host(agents: usize, bytes: u64, fake_agent: &Path) -> Result<HostUsage, String> {
    let host = PtyHost::new();
    let mut ids: Vec<AgentId> = Vec::with_capacity(agents);
    for i in 0..agents {
        let id = host
            .spawn(CommandSpec {
                program: fake_agent.to_path_buf(),
                args: vec![
                    "--bytes".into(),
                    bytes.to_string(),
                    "--ansi".into(),
                    "--seed".into(),
                    i.to_string(),
                ],
                cwd: None,
                env: vec![],
                cols: 120,
                rows: 40,
            })
            .map_err(|e| format!("spawning agent {i}: {e}"))?;
        ids.push(id);
    }
    host.wait_all(Duration::from_secs(120))
        .map_err(|e| e.to_string())?;
    for (i, id) in ids.iter().enumerate() {
        let stats = host
            .stats(*id)
            .ok_or_else(|| format!("agent {i}: no stats"))?;
        // ONLCR on the PTY turns "\n" into "\r\n", so >= not ==.
        if stats.bytes_read < bytes {
            return Err(format!(
                "agent {i}: read {} of {bytes} bytes",
                stats.bytes_read
            ));
        }
        if stats.exit_code != Some(0) {
            return Err(format!("agent {i}: exit code {:?}", stats.exit_code));
        }
    }
    Ok(self_usage())
}

#[cfg(test)]
mod tests {
    use super::{median, minimum};

    #[test]
    fn median_of_odd_count_is_the_middle_value() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
    }

    #[test]
    fn median_of_even_count_is_mean_of_the_two_middle_values() {
        assert_eq!(median(vec![4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn minimum_picks_the_smallest_value() {
        assert_eq!(minimum(vec![3.0, 1.0, 2.0]), 1.0);
    }
}
