//! GUI wall scenarios (macOS window server): `fleet-gui` cold start and
//! input-to-render latency, measured through the GUI's `FLEET_GUI_BENCH`
//! hooks (`crates/fleet-gui/src/app/bench.rs`).
//!
//! The GUI prints one line on stdout when its measurement is done (`ready`,
//! or `input_ns <median>`) and exits 0. It exits 3 when it cannot connect to
//! fleetd, and any other non-zero status (no window server, a timeout) is a
//! failure too: these metrics are never skipped.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use fleet_proto::{KillAgent, SpawnAgent, request, response};

use super::Reaped;
use super::daemon::{COLS, Client, Daemon, ROWS, Streams, spawn_fake};

/// Idle agents listed in the sidebar during `gui_cold_start_ns`.
const COLD_START_AGENTS: usize = 10;
/// Flood agents the GUI shows in panes next to the `cat` agent.
const FLOOD_PANES: usize = 3;
/// Idle agents in the tree `gui_graph_open_ns` opens (the GUI draws them
/// as a synthetic spawned-by tree, `crates/fleet-gui/src/app/bench.rs`).
const GRAPH_OPEN_AGENTS: usize = 50;
/// The GUI's exit status when it cannot connect to fleetd.
const EXIT_NO_FLEETD: i32 = 3;
/// The GUI's exit status when its bench did not finish (no frame presented).
const EXIT_TIMEOUT: i32 = 4;

/// Whether `ioreg -a -n Root -d1` output says the session's screen is locked.
fn locked_in(ioreg: &str) -> bool {
    let mut lines = ioreg.lines().map(str::trim);
    while let Some(line) = lines.next() {
        if line == "<key>CGSSessionScreenIsLocked</key>" {
            return lines.next() == Some("<true/>");
        }
    }
    false
}

/// Fails with "the screen is locked" when it is: a locked session presents
/// no frames, so the GUI metrics cannot be measured (they are never skipped).
pub(super) fn check_screen() -> Result<(), String> {
    let out = Command::new("ioreg")
        .args(["-a", "-n", "Root", "-d1"])
        .output()
        .map_err(|e| format!("running ioreg: {e}"))?;
    if locked_in(&String::from_utf8_lossy(&out.stdout)) {
        return Err(format!(
            "{}: the screen is locked (CGSSessionScreenIsLocked); a locked \
             session presents no frames. Unlock the Mac's GUI session and keep \
             the display awake (caffeinate -d) to measure them",
            super::GUI_METRIC_NAMES.join(", ")
        ));
    }
    Ok(())
}

/// One `fleet-gui` run in bench `mode` against the daemon in `state_dir`.
struct GuiRun<'a> {
    gui: &'a Path,
    state_dir: &'a Path,
    mode: &'a str,
    env: Vec<(&'static str, String)>,
}

impl GuiRun<'_> {
    /// Spawns the GUI and reads stdout until a line starting with `prefix`;
    /// returns that line and the time from just before spawn to reading it.
    /// The GUI must then exit 0.
    fn run(&self, prefix: &str) -> Result<(String, u128), String> {
        check_screen()?;
        let stderr = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(self.gui);
        cmd.arg("--state-dir")
            .arg(self.state_dir)
            .args(["--target", "local"])
            .env("FLEET_GUI_BENCH", self.mode)
            .env_remove("FLEET_GUI_BENCH_AGENT")
            .env_remove("FLEET_GUI_BENCH_EXTRA_PANES")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr.reopen().map_err(|e| e.to_string())?);
        let start = Instant::now();
        let mut child = Reaped(
            cmd.spawn()
                .map_err(|e| format!("spawning {}: {e}", self.gui.display()))?,
        );
        let stdout = child.0.stdout.take().ok_or("fleet-gui stdout not piped")?;
        let mut found = None;
        for line in BufReader::new(stdout).lines() {
            let line = line.map_err(|e| format!("reading fleet-gui output: {e}"))?;
            if line.starts_with(prefix) {
                found = Some((line, start.elapsed().as_nanos()));
                break;
            }
        }
        let status = child
            .0
            .wait()
            .map_err(|e| format!("waiting for fleet-gui: {e}"))?;
        let log = || {
            let mut s = String::new();
            let _ = stderr.reopen().and_then(|mut f| f.read_to_string(&mut s));
            s.trim().to_owned()
        };
        match (found, status.code()) {
            (Some(found), Some(0)) => Ok(found),
            (_, Some(EXIT_TIMEOUT)) => {
                // Locked while it ran?
                check_screen()?;
                Err(format!(
                    "fleet-gui ({}) presented no frame in time: is the screen locked or \
                     the display asleep? {}",
                    self.mode,
                    log()
                ))
            }
            (_, Some(EXIT_NO_FLEETD)) => Err(format!(
                "fleet-gui ({}) could not connect to fleetd: {}",
                self.mode,
                log()
            )),
            (None, _) => Err(format!(
                "fleet-gui ({}) exited with {status} before printing {prefix:?} \
                 (is a window server session available?): {}",
                self.mode,
                log()
            )),
            (Some(_), _) => Err(format!(
                "fleet-gui ({}) exited with {status}: {}",
                self.mode,
                log()
            )),
        }
    }
}

fn spawn_cat(client: &mut Client, streams: &mut Streams) -> Result<u64, String> {
    let spawn = SpawnAgent {
        program: "/bin/cat".into(),
        cols: COLS,
        rows: ROWS,
        ..SpawnAgent::default()
    };
    let req = client.request(request::Kind::Spawn(Box::new(spawn)))?;
    match streams.wait(client, req)? {
        response::Kind::Spawned(info) => Ok(info.id),
        other => Err(format!("spawn cat: unexpected {other:?}")),
    }
}

/// The median the GUI printed as `input_ns <median>`.
fn parse_input_ns(line: &str) -> Result<f64, String> {
    line.strip_prefix("input_ns ")
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| format!("fleet-gui printed {line:?}, not `input_ns <ns>`"))
}

/// `gui_cold_start_ns`: fleetd with [`COLD_START_AGENTS`] idle agents; from
/// just before `fleet-gui` is spawned until it prints `ready` (its first
/// frame drew every listed agent in the sidebar). Minimum of `runs`.
pub(super) fn cold_start_ns(fleetd: &Path, gui: &Path, runs: usize) -> Result<f64, String> {
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    for _ in 0..COLD_START_AGENTS {
        spawn_cat(&mut client, &mut streams)?;
    }
    let run = GuiRun {
        gui,
        state_dir: daemon.state_dir(),
        mode: "cold-start",
        env: vec![],
    };
    let mut best = f64::INFINITY;
    for _ in 0..runs {
        let (_, ns) = run.run("ready")?;
        best = best.min(ns as f64);
    }
    Ok(best)
}

/// `input_to_render_ns`: the GUI's `input` hook against a `/bin/cat` agent
/// on an idle daemon (median of the GUI's samples).
pub(super) fn input_to_render_ns(fleetd: &Path, gui: &Path) -> Result<f64, String> {
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    let cat = spawn_cat(&mut client, &mut streams)?;
    let run = GuiRun {
        gui,
        state_dir: daemon.state_dir(),
        mode: "input",
        env: vec![("FLEET_GUI_BENCH_AGENT", cat.to_string())],
    };
    parse_input_ns(&run.run("input_ns ")?.0)
}

/// `input_to_render_flood50_ns`: the `input` hook while `agents`
/// `fake-agent --ansi --forever` flood (spawned on their own connection,
/// which consumes everything until all exit), the GUI also showing
/// [`FLOOD_PANES`] of them. The floods run until the GUI has printed its
/// result and are then killed, so every sample is taken during the flood;
/// unless `quick`, it is an error if any flood agent exited before that.
///
/// `_bytes` is not used: a fixed-size flood (8 MiB per agent lasts ~3 s)
/// raced the GUI's start, attaches and samples (~1.4 s).
pub(super) fn input_to_render_flood_ns(
    fleetd: &Path,
    gui: &Path,
    fake_agent: &Path,
    agents: usize,
    _bytes: u64,
    quick: bool,
) -> Result<f64, String> {
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    let cat = spawn_cat(&mut client, &mut streams)?;

    let mut flood_client = Client::connect(&daemon.sock)?;
    let mut flood_streams = Streams::default();
    let mut ids = Vec::with_capacity(agents);
    let flood_start = Instant::now();
    for i in 0..agents {
        ids.push(spawn_fake(
            &mut flood_client,
            &mut flood_streams,
            fake_agent,
            &["--forever".into()],
            i,
        )?);
    }
    // Flood agents that have exited so far, with their exit codes.
    let exited: Arc<Mutex<Vec<(u64, i32)>>> = Arc::default();
    let flood = std::thread::spawn({
        let ids: HashSet<u64> = ids.iter().copied().collect();
        let exited = Arc::clone(&exited);
        move || -> Result<(), String> {
            let mut seen = HashSet::new();
            while seen.len() < ids.len() {
                if let Some(r) = flood_streams.take(flood_client.recv()?)? {
                    return Err(format!("unexpected response {r:?}"));
                }
                for (&id, &code) in &flood_streams.exited {
                    if ids.contains(&id) && seen.insert(id) {
                        lock(&exited).push((id, code));
                    }
                }
            }
            Ok(())
        }
    });

    let extra: Vec<String> = ids[..FLOOD_PANES.min(ids.len())]
        .iter()
        .map(u64::to_string)
        .collect();
    let run = GuiRun {
        gui,
        state_dir: daemon.state_dir(),
        mode: "input",
        env: vec![
            ("FLEET_GUI_BENCH_AGENT", cat.to_string()),
            ("FLEET_GUI_BENCH_EXTRA_PANES", extra.join(",")),
        ],
    };
    let result = run.run("input_ns ");
    let gui_done = flood_start.elapsed();
    let early = lock(&exited).clone();
    let line = result?.0;
    for &agent_id in &ids {
        let req = client.request(request::Kind::Kill(KillAgent { agent_id }))?;
        streams.wait(&mut client, req)?;
    }
    flood
        .join()
        .map_err(|_| "flood thread panicked".to_string())??;
    eprintln!(
        "fleet-perf: input to render (flood): GUI done {:.2} s after the first \
         flood spawn, {} of {agents} flood agents still running",
        gui_done.as_secs_f64(),
        agents - early.len()
    );
    if !early.is_empty() {
        let msg = format!(
            "input to render: {} of {agents} flood agents exited before the GUI \
             finished its samples (agent id, exit code): {early:?}",
            early.len()
        );
        if !quick {
            return Err(msg);
        }
        eprintln!("fleet-perf: {msg}");
    }
    parse_input_ns(&line)
}

/// The value the GUI printed as `<name> <ns>` (the graph metrics).
fn parse_ns(line: &str, name: &str) -> Result<f64, String> {
    line.strip_prefix(name)
        .and_then(|v| v.strip_prefix(' '))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| format!("fleet-gui printed {line:?}, not `{name} <ns>`"))
}

/// `gui_graph_open_ns` (ADR 0073): fleetd with [`GRAPH_OPEN_AGENTS`] idle
/// agents; the GUI's `graph-open` hook toggles the graph view on and times
/// it to its first presented frame (median of its samples).
///
/// Not yet called by [`super::run`]: `wall.rs` is gate code, so wiring the
/// metric in needs the user's approval (see ADR 0073).
#[allow(dead_code)]
pub(super) fn graph_open_ns(fleetd: &Path, gui: &Path) -> Result<f64, String> {
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    for _ in 0..GRAPH_OPEN_AGENTS {
        spawn_cat(&mut client, &mut streams)?;
    }
    let run = GuiRun {
        gui,
        state_dir: daemon.state_dir(),
        mode: "graph-open",
        env: vec![],
    };
    parse_ns(&run.run("graph_open_ns ")?.0, "graph_open_ns")
}

/// `gui_graph_frame_flood50_ns` (ADR 0073): the graph view showing
/// `agents` `fake-agent --ansi --forever` while they flood (spawned and
/// consumed on their own connection); the GUI's `graph-flood` hook times
/// window redraws from the workspace's render to the end of the graph's
/// paint (median). The floods are killed once the GUI has printed; unless
/// `quick`, it is an error if any exited before.
///
/// Not yet called by [`super::run`] (see [`graph_open_ns`]).
#[allow(dead_code)]
pub(super) fn graph_frame_flood_ns(
    fleetd: &Path,
    gui: &Path,
    fake_agent: &Path,
    agents: usize,
    quick: bool,
) -> Result<f64, String> {
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    let flood = ForeverFlood::start(&daemon, fake_agent, agents)?;
    let run = GuiRun {
        gui,
        state_dir: daemon.state_dir(),
        mode: "graph-flood",
        env: vec![("FLEET_GUI_BENCH_AGENTS", agents.to_string())],
    };
    let result = run.run("graph_frame_ns ");
    let early = flood.stop(&mut client, &mut streams, "graph frame")?;
    let line = result?.0;
    if !early.is_empty() {
        let msg = format!(
            "graph frame: {} of {agents} flood agents exited before the GUI \
             finished its samples (agent id, exit code): {early:?}",
            early.len()
        );
        if !quick {
            return Err(msg);
        }
        eprintln!("fleet-perf: {msg}");
    }
    parse_ns(&line, "graph_frame_ns")
}

/// `agents` `fake-agent --forever` spawned on their own connection, whose
/// output a thread consumes until they all exit.
struct ForeverFlood {
    ids: Vec<u64>,
    /// Flood agents that have exited so far, with their exit codes.
    exited: Arc<Mutex<Vec<(u64, i32)>>>,
    thread: std::thread::JoinHandle<Result<(), String>>,
    start: Instant,
}

impl ForeverFlood {
    fn start(daemon: &Daemon, fake_agent: &Path, agents: usize) -> Result<Self, String> {
        let mut flood_client = Client::connect(&daemon.sock)?;
        let mut flood_streams = Streams::default();
        let mut ids = Vec::with_capacity(agents);
        let start = Instant::now();
        for i in 0..agents {
            ids.push(spawn_fake(
                &mut flood_client,
                &mut flood_streams,
                fake_agent,
                &["--forever".into()],
                i,
            )?);
        }
        let exited: Arc<Mutex<Vec<(u64, i32)>>> = Arc::default();
        let thread = std::thread::spawn({
            let ids: HashSet<u64> = ids.iter().copied().collect();
            let exited = Arc::clone(&exited);
            move || -> Result<(), String> {
                let mut seen = HashSet::new();
                while seen.len() < ids.len() {
                    if let Some(r) = flood_streams.take(flood_client.recv()?)? {
                        return Err(format!("unexpected response {r:?}"));
                    }
                    for (&id, &code) in &flood_streams.exited {
                        if ids.contains(&id) && seen.insert(id) {
                            lock(&exited).push((id, code));
                        }
                    }
                }
                Ok(())
            }
        });
        Ok(ForeverFlood {
            ids,
            exited,
            thread,
            start,
        })
    }

    /// Kills every flood agent (on `client`) and waits for the consumer;
    /// returns the agents that had exited before.
    fn stop(
        self,
        client: &mut Client,
        streams: &mut Streams,
        what: &str,
    ) -> Result<Vec<(u64, i32)>, String> {
        let done = self.start.elapsed();
        let early = lock(&self.exited).clone();
        for &agent_id in &self.ids {
            let req = client.request(request::Kind::Kill(KillAgent { agent_id }))?;
            streams.wait(client, req)?;
        }
        self.thread
            .join()
            .map_err(|_| "flood thread panicked".to_string())??;
        eprintln!(
            "fleet-perf: {what} (flood): GUI done {:.2} s after the first flood spawn, \
             {} of {} flood agents still running",
            done.as_secs_f64(),
            self.ids.len() - early.len(),
            self.ids.len()
        );
        Ok(early)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{locked_in, parse_input_ns, parse_ns};

    #[test]
    fn locked_screen_is_read_from_ioreg() {
        let plist = |v: &str| {
            format!(
                "<dict>\n\t<key>CGSSessionScreenIsLocked</key>\n\t{v}\n\t<key>kCGSSessionOnConsoleKey</key>\n\t<true/>\n</dict>"
            )
        };
        assert!(locked_in(&plist("<true/>")));
        assert!(!locked_in(&plist("<false/>")));
        // An unlocked session has no such key at all.
        assert!(!locked_in(
            "<dict>\n\t<key>kCGSSessionOnConsoleKey</key>\n\t<true/>\n</dict>"
        ));
    }

    #[test]
    fn input_ns_line_is_parsed() {
        assert_eq!(parse_input_ns("input_ns 4200000"), Ok(4_200_000.0));
        assert!(parse_input_ns("input_ns").is_err());
        assert!(parse_input_ns("ready").is_err());
    }

    #[test]
    fn graph_lines_are_parsed() {
        assert_eq!(
            parse_ns("graph_open_ns 31000000", "graph_open_ns"),
            Ok(31e6)
        );
        assert_eq!(
            parse_ns("graph_frame_ns 2500000", "graph_frame_ns"),
            Ok(2.5e6)
        );
        assert!(parse_ns("graph_open_ns_x 1", "graph_open_ns").is_err());
        assert!(parse_ns("graph_frame_ns 1", "graph_open_ns").is_err());
    }
}
