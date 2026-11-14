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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use fleet_proto::{SpawnAgent, request, response};

use super::Reaped;
use super::daemon::{COLS, Client, Daemon, ROWS, Streams, spawn_fake_agent};

/// Idle agents listed in the sidebar during `gui_cold_start_ns`.
const COLD_START_AGENTS: usize = 10;
/// Flood agents the GUI shows in panes next to the `cat` agent.
const FLOOD_PANES: usize = 3;
/// The GUI's exit status when it cannot connect to fleetd.
const EXIT_NO_FLEETD: i32 = 3;

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
            (_, Some(EXIT_NO_FLEETD)) => Err(format!(
                "fleet-gui ({}) could not connect to fleetd: {}",
                self.mode,
                log()
            )),
            (None, _) => Err(format!(
                "fleet-gui ({}) exited with {status} before printing {prefix:?} \
                 (is a window server session available, with the screen unlocked?): {}",
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
/// `fake-agent --ansi --bytes <bytes>` flood (spawned on their own
/// connection, which consumes everything until all exit), the GUI also
/// showing [`FLOOD_PANES`] of them. Unless `quick`, the GUI must finish
/// its samples before the flood ends.
pub(super) fn input_to_render_flood_ns(
    fleetd: &Path,
    gui: &Path,
    fake_agent: &Path,
    agents: usize,
    bytes: u64,
    quick: bool,
) -> Result<f64, String> {
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    let cat = spawn_cat(&mut client, &mut streams)?;

    let mut flood_client = Client::connect(&daemon.sock)?;
    let mut flood_streams = Streams::default();
    let mut ids = Vec::with_capacity(agents);
    for i in 0..agents {
        ids.push(spawn_fake_agent(
            &mut flood_client,
            &mut flood_streams,
            fake_agent,
            bytes,
            i,
        )?);
    }
    let done = Arc::new(AtomicBool::new(false));
    let flood = std::thread::spawn({
        let ids: HashSet<u64> = ids.iter().copied().collect();
        let done = Arc::clone(&done);
        move || -> Result<(), String> {
            let r = (|| {
                while !ids.iter().all(|id| flood_streams.exited.contains_key(id)) {
                    if let Some(r) = flood_streams.take(flood_client.recv()?)? {
                        return Err(format!("unexpected response {r:?}"));
                    }
                }
                Ok(())
            })();
            done.store(true, Ordering::Release);
            r?;
            match flood_streams.exited.iter().find(|(_, code)| **code != 0) {
                Some((id, code)) => Err(format!("agent {id} exited with {code}")),
                None => Ok(()),
            }
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
    let during_flood = !done.load(Ordering::Acquire);
    let line = result?.0;
    flood
        .join()
        .map_err(|_| "flood thread panicked".to_string())??;
    if !during_flood {
        let msg = "input to render: the flood ended before the GUI finished its samples";
        if !quick {
            return Err(msg.into());
        }
        eprintln!("fleet-perf: {msg}");
    }
    parse_input_ns(&line)
}

#[cfg(test)]
mod tests {
    use super::parse_input_ns;

    #[test]
    fn input_ns_line_is_parsed() {
        assert_eq!(parse_input_ns("input_ns 4200000"), Ok(4_200_000.0));
        assert!(parse_input_ns("input_ns").is_err());
        assert!(parse_input_ns("ready").is_err());
    }
}
