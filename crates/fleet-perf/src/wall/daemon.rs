//! Wall scenarios through the `fleetd` protocol server: the 50-agent flood with
//! one client attached to every agent, the flood with one client attached to
//! four agents (and input echo latency through it), and reattach (snapshot)
//! latency.
//!
//! The daemon's CPU time and RSS are sampled per process (`proc_pid_rusage` on
//! macOS, `/proc/<pid>/{stat,statm}` on Linux), so its agents' CPU is excluded.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fleet_proto::{
    Attach, Detach, Frame, FrameCodec, Hello, KillAgent, PROTOCOL_VERSION, Request, SendInput,
    SpawnAgent, event, request, response,
};
use tokio_util::codec::{Decoder, Encoder};

use super::{MIB, Reaped, median, spawn_fleetd, wait_ready};

pub(super) const COLS: u32 = 120;
pub(super) const ROWS: u32 = 40;
/// Bound on any single blocking read from the daemon.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const READ_CHUNK: usize = 1024 * 1024;
const RSS_SAMPLE_EVERY: Duration = Duration::from_millis(50);
/// How long the reattach agent may take to print its output.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Printed by the reattach agent once its output is done. Contains `-`, which
/// fake-agent output never does.
const READY_MARK: &str = "fleet-perf-ready";
/// Grace for `fleetd` to stop its agents after SIGTERM.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Blocking protocol client over the daemon's unix socket.
pub(super) struct Client {
    stream: UnixStream,
    codec: FrameCodec,
    rbuf: BytesMut,
    chunk: Vec<u8>,
    wbuf: BytesMut,
    next_id: u64,
}

impl Client {
    pub(super) fn connect(sock: &Path) -> Result<Client, String> {
        let stream = UnixStream::connect(sock).map_err(|e| format!("connecting: {e}"))?;
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let mut client = Client {
            stream,
            codec: FrameCodec::new(),
            rbuf: BytesMut::new(),
            chunk: vec![0; READ_CHUNK],
            wbuf: BytesMut::new(),
            next_id: 1,
        };
        client.send(Frame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client: "fleet-perf".into(),
            ..Default::default()
        }))?;
        match client.recv()? {
            Frame::HelloOk(_) => Ok(client),
            other => Err(format!("handshake: unexpected {other:?}")),
        }
    }

    fn send(&mut self, frame: Frame) -> Result<(), String> {
        self.wbuf.clear();
        self.codec
            .encode(frame, &mut self.wbuf)
            .map_err(|e| format!("encoding: {e}"))?;
        self.stream
            .write_all(&self.wbuf)
            .map_err(|e| format!("writing to fleetd: {e}"))
    }

    /// Sends a request; returns its id.
    pub(super) fn request(&mut self, kind: request::Kind) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(Frame::Request(Request {
            id,
            kind: Some(kind),
        }))?;
        Ok(id)
    }

    pub(super) fn recv(&mut self) -> Result<Frame, String> {
        loop {
            if let Some(frame) = self
                .codec
                .decode(&mut self.rbuf)
                .map_err(|e| format!("decoding: {e}"))?
            {
                return Ok(frame);
            }
            let n = self
                .stream
                .read(&mut self.chunk)
                .map_err(|e| format!("reading from fleetd: {e}"))?;
            if n == 0 {
                return Err("fleetd closed the connection".into());
            }
            self.rbuf.extend_from_slice(&self.chunk[..n]);
        }
    }
}

/// Stream state of one connection: everything that is not a response.
#[derive(Default)]
pub(super) struct Streams {
    /// Agent id -> exit code, from `AgentExited` events.
    pub(super) exited: HashMap<u64, i32>,
    /// Subscriptions the daemon dropped for falling behind.
    lagged: usize,
}

impl Streams {
    /// Consumes a stream frame; returns a response that is not a lag notice.
    pub(super) fn take(&mut self, frame: Frame) -> Result<Option<fleet_proto::Response>, String> {
        match frame {
            Frame::Output { .. } => Ok(None),
            Frame::Event(e) => {
                match e.kind {
                    Some(event::Kind::Exited(x)) => {
                        self.exited.insert(x.agent_id, x.exit_code);
                    }
                    Some(event::Kind::Lagged(_)) => self.lagged += 1,
                    _ => {}
                }
                Ok(None)
            }
            Frame::Response(r) => Ok(Some(r)),
            other => Err(format!("unexpected frame {other:?}")),
        }
    }

    /// Reads until the response to `id`, consuming stream frames on the way.
    pub(super) fn wait(&mut self, client: &mut Client, id: u64) -> Result<response::Kind, String> {
        loop {
            let Some(r) = self.take(client.recv()?)? else {
                continue;
            };
            if r.id != id {
                return Err(format!("response {} while waiting for {id}", r.id));
            }
            return match r.kind {
                Some(response::Kind::Error(e)) => Err(format!("request {id}: {}", e.message)),
                Some(kind) => Ok(kind),
                None => Err(format!("request {id}: empty response")),
            };
        }
    }
}

/// A running `fleetd` with its own state dir; stopped with SIGTERM on drop
/// (so it stops its agents), then killed if it does not exit in time.
pub(super) struct Daemon {
    process: Reaped,
    pub(super) sock: std::path::PathBuf,
    dir: tempfile::TempDir,
}

impl Daemon {
    pub(super) fn start(fleetd: &Path) -> Result<Daemon, String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let sock = dir.path().join("fleetd.sock");
        let mut process = spawn_fleetd(fleetd, dir.path())?;
        wait_ready(&mut process, &sock)?;
        Ok(Daemon { process, sock, dir })
    }

    /// Its `--state-dir` (holds `fleetd.sock`).
    pub(super) fn state_dir(&self) -> &Path {
        self.dir.path()
    }

    fn pid(&self) -> u32 {
        self.process.0.id()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: plain signal to our own child.
        unsafe { libc::kill(self.pid() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if !matches!(self.process.0.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // `Reaped` kills and reaps it.
    }
}

/// CPU time (user + system, this process only) and resident set of `pid`.
#[derive(Debug, Clone, Copy)]
pub struct ProcUsage {
    pub cpu_ns: u64,
    pub rss_bytes: u64,
}

#[cfg(target_os = "macos")]
pub fn proc_usage(pid: u32) -> Result<ProcUsage, String> {
    // SAFETY: zeroed rusage_info_v2 is a valid out-parameter of the V2 flavor.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V2,
            (&mut info as *mut libc::rusage_info_v2).cast::<libc::rusage_info_t>(),
        )
    };
    if rc != 0 {
        return Err(format!(
            "proc_pid_rusage({pid}): {}",
            std::io::Error::last_os_error()
        ));
    }
    // Times are in Mach absolute-time units (not ns on Apple silicon).
    #[allow(deprecated)]
    let (numer, denom) = {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: valid out-parameter.
        unsafe { libc::mach_timebase_info(&mut tb) };
        (u128::from(tb.numer), u128::from(tb.denom.max(1)))
    };
    let ticks = u128::from(info.ri_user_time + info.ri_system_time);
    Ok(ProcUsage {
        cpu_ns: (ticks * numer / denom) as u64,
        rss_bytes: info.ri_resident_size,
    })
}

#[cfg(target_os = "linux")]
pub fn proc_usage(pid: u32) -> Result<ProcUsage, String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|e| format!("reading stat: {e}"))?;
    // Fields after the parenthesised command name start at field 3 (state);
    // utime and stime are fields 14 and 15.
    let rest = stat.rsplit_once(')').ok_or("malformed stat")?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |i: usize| -> Result<u64, String> {
        fields
            .get(i)
            .and_then(|f| f.parse().ok())
            .ok_or_else(|| "malformed stat".to_string())
    };
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let cpu_ticks = ticks(11)? + ticks(12)?;
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .map_err(|e| format!("reading statm: {e}"))?;
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .ok_or("malformed statm")?;
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Ok(ProcUsage {
        cpu_ns: cpu_ticks * 1_000_000_000 / hz,
        rss_bytes: pages * page,
    })
}

/// Samples `pid`'s RSS every [`RSS_SAMPLE_EVERY`] until dropped.
struct RssSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RssSampler {
    fn start(pid: u32) -> RssSampler {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let thread = std::thread::spawn({
            let (stop, peak) = (Arc::clone(&stop), Arc::clone(&peak));
            move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(u) = proc_usage(pid) {
                        peak.fetch_max(u.rss_bytes, Ordering::Relaxed);
                    }
                    std::thread::sleep(RSS_SAMPLE_EVERY);
                }
            }
        });
        RssSampler {
            stop,
            peak,
            thread: Some(thread),
        }
    }

    /// Stops sampling; the peak includes `last`, a final sample.
    fn finish(mut self, last: u64) -> u64 {
        self.stop_thread();
        self.peak.load(Ordering::Relaxed).max(last)
    }

    fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RssSampler {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

pub(super) struct DaemonFlood {
    pub wall_ns: f64,
    pub cpu_ns_per_mib: f64,
    pub peak_rss_bytes: f64,
    /// Subscriptions fleetd dropped as lagged during the run.
    pub lagged_subscriptions: f64,
}

/// One daemon flood: `agents` fake agents spawned through the protocol, each
/// attached on one connection (snapshot plus stream consumed) until all have
/// exited. Wall time runs from the first spawn request to the last exit. A
/// subscription fleetd drops as lagged is not re-attached (the lag notice does
/// not name the agent); its exit event still arrives.
pub(super) fn flood(
    fleetd: &Path,
    fake_agent: &Path,
    agents: usize,
    bytes: u64,
) -> Result<DaemonFlood, String> {
    let daemon = Daemon::start(fleetd)?;
    let pid = daemon.pid();
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    let sampler = RssSampler::start(pid);
    let cpu_before = proc_usage(pid)?.cpu_ns;

    let start = Instant::now();
    let mut ids = HashSet::with_capacity(agents);
    for i in 0..agents {
        let spawn = SpawnAgent {
            program: fake_agent.to_string_lossy().into_owned(),
            args: vec![
                "--bytes".into(),
                bytes.to_string(),
                "--ansi".into(),
                "--seed".into(),
                i.to_string(),
            ],
            cols: COLS,
            rows: ROWS,
            ..SpawnAgent::default()
        };
        let req = client.request(request::Kind::Spawn(Box::new(spawn)))?;
        let agent_id = match streams.wait(&mut client, req)? {
            response::Kind::Spawned(info) => info.id,
            other => return Err(format!("spawn: unexpected {other:?}")),
        };
        ids.insert(agent_id);
        let req = client.request(request::Kind::Attach(Attach {
            agent_id,
            observe: false,
        }))?;
        match streams.wait(&mut client, req)? {
            response::Kind::Attached(_) => {}
            other => return Err(format!("attach: unexpected {other:?}")),
        }
    }
    while !ids.iter().all(|id| streams.exited.contains_key(id)) {
        if let Some(r) = streams.take(client.recv()?)? {
            return Err(format!("unexpected response {r:?}"));
        }
    }
    let wall_ns = start.elapsed().as_nanos() as f64;
    let end = proc_usage(pid)?;
    let peak = sampler.finish(end.rss_bytes);

    if let Some((id, code)) = streams.exited.iter().find(|(_, code)| **code != 0) {
        return Err(format!("agent {id} exited with {code}"));
    }
    if streams.lagged > 0 {
        // Under full load fleetd drops subscribers it cannot forward to in
        // time (its drop policy, not this client: it keeps up). The run still
        // counts; the number is reported so the scenario's shape is visible.
        eprintln!(
            "fleet-perf: daemon flood: fleetd dropped {} of {agents} subscriptions as lagged",
            streams.lagged
        );
    }
    let total_mib = (agents as u64 * bytes) as f64 / MIB;
    Ok(DaemonFlood {
        wall_ns,
        cpu_ns_per_mib: end.cpu_ns.saturating_sub(cpu_before) as f64 / total_mib,
        peak_rss_bytes: peak as f64,
        lagged_subscriptions: streams.lagged as f64,
    })
}

/// Median latency from `Attach` sent to `AttachOk` received, over `attaches`
/// attaches (detaching in between) to an idle agent that printed `bytes`.
pub(super) fn reattach_snapshot_ns(
    fleetd: &Path,
    fake_agent: &Path,
    bytes: u64,
    attaches: usize,
) -> Result<f64, String> {
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    // The output may end inside an escape sequence; ESC aborts it and SGR 0
    // resets the pen, so the mark lands on the grid verbatim.
    let script = format!(
        "\"$0\" --ansi --bytes {bytes} && printf '\\033[0m\\r\\n%s\\r\\n' {READY_MARK} && exec sleep 600"
    );
    let spawn = SpawnAgent {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            script,
            fake_agent.to_string_lossy().into_owned(),
        ],
        cols: COLS,
        rows: ROWS,
        ..SpawnAgent::default()
    };
    let req = client.request(request::Kind::Spawn(Box::new(spawn)))?;
    let agent_id = match streams.wait(&mut client, req)? {
        response::Kind::Spawned(info) => info.id,
        other => return Err(format!("spawn: unexpected {other:?}")),
    };

    let attach = |client: &mut Client, streams: &mut Streams| -> Result<Vec<u8>, String> {
        let req = client.request(request::Kind::Attach(Attach {
            agent_id,
            observe: false,
        }))?;
        let snapshot = match streams.wait(client, req)? {
            response::Kind::Attached(ok) => ok.snapshot,
            other => return Err(format!("attach: unexpected {other:?}")),
        };
        let req = client.request(request::Kind::Detach(Detach { agent_id }))?;
        streams.wait(client, req)?;
        Ok(snapshot)
    };

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let snapshot = attach(&mut client, &mut streams)?;
        if snapshot
            .windows(READY_MARK.len())
            .any(|w| w == READY_MARK.as_bytes())
        {
            break;
        }
        if streams.exited.contains_key(&agent_id) {
            return Err("reattach agent exited before it was ready".into());
        }
        if Instant::now() >= deadline {
            return Err(format!("reattach agent not ready within {READY_TIMEOUT:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut samples = Vec::with_capacity(attaches);
    for _ in 0..attaches {
        let req = client.request(request::Kind::Attach(Attach {
            agent_id,
            observe: false,
        }))?;
        let sent = Instant::now();
        match streams.wait(&mut client, req)? {
            response::Kind::Attached(_) => {}
            other => return Err(format!("attach: unexpected {other:?}")),
        }
        samples.push(sent.elapsed().as_nanos() as f64);
        let req = client.request(request::Kind::Detach(Detach { agent_id }))?;
        streams.wait(&mut client, req)?;
    }

    let req = client.request(request::Kind::Kill(KillAgent { agent_id }))?;
    streams.wait(&mut client, req)?;
    Ok(median(samples))
}

/// `exec_true_roundtrip_ns` (ADR 0091): median latency from `Exec
/// /usr/bin/true` sent to its `ExecResult` received, over `execs` calls on
/// one connection to an idle daemon (after a few warm-up calls).
pub(super) fn exec_true_roundtrip_ns(fleetd: &Path, execs: usize) -> Result<f64, String> {
    const WARMUP: usize = 3;
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let mut samples = Vec::with_capacity(execs);
    for i in 0..WARMUP + execs {
        let exec = fleet_proto::Exec {
            argv: vec!["/usr/bin/true".into()],
            cwd: cwd.clone(),
            ..Default::default()
        };
        let sent = Instant::now();
        let req = client.request(request::Kind::Exec(Box::new(exec)))?;
        match streams.wait(&mut client, req)? {
            response::Kind::ExecResult(r) if r.exit_code == 0 => {}
            other => return Err(format!("exec true: unexpected {other:?}")),
        }
        if i >= WARMUP {
            samples.push(sent.elapsed().as_nanos() as f64);
        }
    }
    Ok(median(samples))
}

/// Spawns one `fake-agent --ansi --bytes <bytes> --seed <seed>`; returns its id.
pub(super) fn spawn_fake_agent(
    client: &mut Client,
    streams: &mut Streams,
    fake_agent: &Path,
    bytes: u64,
    seed: usize,
) -> Result<u64, String> {
    spawn_fake(
        client,
        streams,
        fake_agent,
        &["--bytes".into(), bytes.to_string()],
        seed,
    )
}

/// Spawns one `fake-agent <len> --ansi --seed <seed>`, where `len` is
/// `--bytes <n>` or `--forever`; returns its id.
pub(super) fn spawn_fake(
    client: &mut Client,
    streams: &mut Streams,
    fake_agent: &Path,
    len: &[String],
    seed: usize,
) -> Result<u64, String> {
    let mut args = len.to_vec();
    args.extend(["--ansi".into(), "--seed".into(), seed.to_string()]);
    let spawn = SpawnAgent {
        program: fake_agent.to_string_lossy().into_owned(),
        args,
        cols: COLS,
        rows: ROWS,
        ..SpawnAgent::default()
    };
    let req = client.request(request::Kind::Spawn(Box::new(spawn)))?;
    match streams.wait(client, req)? {
        response::Kind::Spawned(info) => Ok(info.id),
        other => Err(format!("spawn: unexpected {other:?}")),
    }
}

fn attach_agent(client: &mut Client, streams: &mut Streams, agent_id: u64) -> Result<(), String> {
    let req = client.request(request::Kind::Attach(Attach {
        agent_id,
        observe: false,
    }))?;
    match streams.wait(client, req)? {
        response::Kind::Attached(_) => Ok(()),
        other => Err(format!("attach: unexpected {other:?}")),
    }
}

/// The flood with a few attached agents (a GUI showing panes): `agents` fake
/// agents spawned on one connection, which attaches only the first
/// `attached` and consumes everything until all have exited. `spawned` is
/// set once every agent is spawned and attached, `done` once all have
/// exited. Returns the subscriptions fleetd dropped as lagged.
fn flood_attached_on(
    sock: &Path,
    fake_agent: &Path,
    agents: usize,
    attached: usize,
    bytes: u64,
    spawned: &AtomicBool,
    done: &AtomicBool,
) -> Result<usize, String> {
    let mut client = Client::connect(sock)?;
    let mut streams = Streams::default();
    let mut ids = HashSet::with_capacity(agents);
    for i in 0..agents {
        let agent_id = spawn_fake_agent(&mut client, &mut streams, fake_agent, bytes, i)?;
        ids.insert(agent_id);
        if i < attached {
            attach_agent(&mut client, &mut streams, agent_id)?;
        }
    }
    spawned.store(true, Ordering::Release);
    while !ids.iter().all(|id| streams.exited.contains_key(id)) {
        if let Some(r) = streams.take(client.recv()?)? {
            return Err(format!("unexpected response {r:?}"));
        }
    }
    done.store(true, Ordering::Release);
    if let Some((id, code)) = streams.exited.iter().find(|(_, code)| **code != 0) {
        return Err(format!("agent {id} exited with {code}"));
    }
    Ok(streams.lagged)
}

/// `flood50_attached4_lagged_subscriptions`: subscriptions fleetd dropped as
/// lagged while `agents` agents flood and one client is attached to the
/// first `attached` of them, consuming everything.
pub(super) fn flood_attached_lagged(
    fleetd: &Path,
    fake_agent: &Path,
    agents: usize,
    attached: usize,
    bytes: u64,
) -> Result<f64, String> {
    let daemon = Daemon::start(fleetd)?;
    let (spawned, done) = (AtomicBool::new(false), AtomicBool::new(false));
    let lagged = flood_attached_on(
        &daemon.sock,
        fake_agent,
        agents,
        attached,
        bytes,
        &spawned,
        &done,
    )?;
    if lagged > 0 {
        eprintln!(
            "fleet-perf: flood with {attached} attached: fleetd dropped {lagged} of {attached} subscriptions as lagged"
        );
    }
    Ok(lagged as f64)
}

/// Outcome of [`input_echo_ns`].
pub(super) struct InputEcho {
    /// Median latency from `SendInput` written to the echo's `Output` read.
    pub median_ns: f64,
    /// Samples that completed while the flood was still running.
    pub during_flood: usize,
}

/// `input_echo_flood50_ns`: the flood of [`flood_attached_lagged`] on one
/// connection plus, on its own connection, an attached `cat` agent. Once
/// every flood agent is spawned, `samples` single-byte `SendInput`s are sent
/// to `cat` one at a time (`gap` apart); each sample runs from the request
/// written to the first `Output` of `cat` holding that byte (the PTY's
/// echo). The flood connection still has to finish cleanly.
pub(super) fn input_echo_ns(
    fleetd: &Path,
    fake_agent: &Path,
    agents: usize,
    attached: usize,
    bytes: u64,
    samples: usize,
    gap: Duration,
) -> Result<InputEcho, String> {
    let daemon = Daemon::start(fleetd)?;
    let mut client = Client::connect(&daemon.sock)?;
    let mut streams = Streams::default();
    let spawn = SpawnAgent {
        program: "/bin/cat".into(),
        cols: COLS,
        rows: ROWS,
        ..SpawnAgent::default()
    };
    let req = client.request(request::Kind::Spawn(Box::new(spawn)))?;
    let cat = match streams.wait(&mut client, req)? {
        response::Kind::Spawned(info) => info.id,
        other => return Err(format!("spawn cat: unexpected {other:?}")),
    };
    attach_agent(&mut client, &mut streams, cat)?;

    let (spawned, done) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let flood = std::thread::spawn({
        let (sock, fake_agent) = (daemon.sock.clone(), fake_agent.to_path_buf());
        let (spawned, done) = (Arc::clone(&spawned), Arc::clone(&done));
        move || {
            let r = flood_attached_on(&sock, &fake_agent, agents, attached, bytes, &spawned, &done);
            // Unblock the sampler if the flood failed early.
            spawned.store(true, Ordering::Release);
            r
        }
    });
    while !spawned.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut latencies = Vec::with_capacity(samples);
    let mut during_flood = 0;
    for i in 0..samples {
        let byte = b'a' + (i % 26) as u8;
        let req = client.request(request::Kind::Input(SendInput {
            agent_id: cat,
            data: vec![byte],
        }))?;
        let sent = Instant::now();
        let (mut echoed, mut acked) = (false, false);
        while !(echoed && acked) {
            match client.recv()? {
                Frame::Output { agent_id, data } if agent_id == cat => {
                    if !echoed && data.contains(&byte) {
                        latencies.push(sent.elapsed().as_nanos() as f64);
                        echoed = true;
                    }
                }
                frame => {
                    if let Some(r) = streams.take(frame)? {
                        match r.kind {
                            Some(response::Kind::Ack(_)) if r.id == req => acked = true,
                            other => return Err(format!("input {req}: unexpected {other:?}")),
                        }
                    }
                }
            }
            if streams.exited.contains_key(&cat) {
                return Err("cat exited during the echo samples".into());
            }
        }
        if !done.load(Ordering::Acquire) {
            during_flood += 1;
        }
        std::thread::sleep(gap);
    }
    let lagged = flood
        .join()
        .map_err(|_| "flood thread panicked".to_string())??;
    if streams.lagged > 0 {
        return Err("fleetd dropped the cat subscription as lagged".into());
    }
    if lagged > 0 {
        eprintln!(
            "fleet-perf: input echo: fleetd dropped {lagged} of {attached} flood subscriptions as lagged"
        );
    }
    Ok(InputEcho {
        median_ns: median(latencies),
        during_flood,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_cpu_ns() -> u64 {
        // SAFETY: zeroed rusage is a valid out-parameter.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        super::super::tv_ns(ru.ru_utime) + super::super::tv_ns(ru.ru_stime)
    }

    #[test]
    fn proc_usage_cpu_is_in_nanoseconds() {
        let burn = Instant::now();
        let mut x = 0u64;
        while burn.elapsed() < Duration::from_millis(300) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let ours = proc_usage(std::process::id()).unwrap();
        let rusage = self_cpu_ns();
        assert!(ours.rss_bytes > 0);
        // Same clock, read moments apart; a unit error would be off by 10x+.
        let ratio = ours.cpu_ns as f64 / rusage as f64;
        assert!(
            (0.8..1.25).contains(&ratio),
            "proc_usage {} ns vs getrusage {rusage} ns",
            ours.cpu_ns
        );
    }
}
