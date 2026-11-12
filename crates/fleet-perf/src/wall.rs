//! Wall-clock suite: fleetd cold start, idle RSS, and the 50-agent flood.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fleet_node::pty::{AgentId, CommandSpec, PtyHost};
use serde::{Deserialize, Serialize};

use crate::results::{Measurement, Results, Suite};

const FLOOD_AGENTS: usize = 50;
const FLOOD_BYTES: u64 = 8 * 1024 * 1024;
const QUICK_FLOOD_BYTES: u64 = 256 * 1024;
const MIB: f64 = 1024.0 * 1024.0;
const READY_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Poll every millisecond until the daemon socket accepts a connection.
fn wait_ready(daemon: &mut Reaped, sock: &Path) -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
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
        std::thread::sleep(Duration::from_millis(1));
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
    v[v.len() / 2]
}

fn repeat(n: usize, mut f: impl FnMut() -> Result<f64, String>) -> Result<f64, String> {
    (0..n)
        .map(|_| f())
        .collect::<Result<Vec<_>, _>>()
        .map(median)
}

pub fn run(quick: bool) -> Result<Results, String> {
    let fleetd = sibling("fleetd")?;
    let (cold_runs, rss_runs, flood_runs, bytes) = if quick {
        (1, 1, 1, QUICK_FLOOD_BYTES)
    } else {
        (20, 5, 5, FLOOD_BYTES)
    };

    let cold = repeat(cold_runs, || cold_start_ns(&fleetd))?;
    let rss = repeat(rss_runs, || idle_rss_bytes(&fleetd))?;
    let mut floods = Vec::new();
    for _ in 0..flood_runs {
        floods.push(flood(bytes)?);
    }
    let pick = |f: fn(&Flood) -> f64| median(floods.iter().map(f).collect());

    let m = |value: f64, unit: &str| Measurement {
        value,
        unit: unit.to_string(),
    };
    let metrics = BTreeMap::from([
        ("fleetd_cold_start_ns".to_string(), m(cold, "ns")),
        ("fleetd_idle_rss_bytes".to_string(), m(rss, "bytes")),
        ("flood50_wall_ns".to_string(), m(pick(|f| f.wall_ns), "ns")),
        (
            "flood50_cpu_ns_per_mib".to_string(),
            m(pick(|f| f.cpu_ns_per_mib), "ns"),
        ),
        (
            "flood50_peak_rss_bytes".to_string(),
            m(pick(|f| f.peak_rss_bytes), "bytes"),
        ),
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
    // Spawn from a single thread: macOS `ptsname` is not reentrant.
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
