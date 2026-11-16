//! `fleet lease` (ADR 0077, spec §4): named locks for shared resources.
//!
//! `fleet lease <name> … -- <cmd>` holds the lease on its connection while
//! `cmd` runs and exits with its code; `acquire` / `release` are for longer
//! holds; `ls` lists. Exit codes (spec §1): 0 ok, 1 busy / not held, 2
//! usage or other error, 3 forbidden.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args as ClapArgs, Subcommand};
use fleet_client::{Connection, Error, Result};
use fleet_proto::{AcquireLease, LeaseInfo, ListLeases, ReleaseLease, request, response};
use serde_json::{Value, json};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

/// How long a client keeps trying to reach a daemon that is restarting.
const RECONNECT_FOR: Duration = Duration::from_secs(45);
const RECONNECT_EVERY: Duration = Duration::from_millis(250);
/// `--wait` says it is waiting only when the answer takes longer than this.
const WAITING_NOTICE_AFTER: Duration = Duration::from_millis(400);

#[derive(ClapArgs, Debug)]
#[command(
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true,
    after_help = "A lease named `acquire`, `release` or `ls` cannot use the `--` form; \
                  take it with `fleet lease acquire <name>`."
)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Option<Sub>,
    /// Lease name (per node; any caller may use any name)
    #[arg(required = true)]
    pub name: Option<String>,
    /// Grant only while the 1-minute load average is below N
    #[arg(long, value_name = "N")]
    pub max_load: Option<f64>,
    /// Release after this long, e.g. 90s, 30m, 1h, 1h30m, 2d
    #[arg(long, value_parser = parse_ttl, value_name = "DURATION")]
    pub ttl: Option<u64>,
    /// Queue behind the holder instead of failing (exit 1) when it is held
    #[arg(long)]
    pub wait: bool,
    /// The command to run while holding the lease; its exit code is ours
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Sub {
    /// Take a lease without a command (the user's needs --ttl; an agent's
    /// ends when the agent does)
    Acquire {
        name: String,
        #[arg(long, value_name = "N")]
        max_load: Option<f64>,
        #[arg(long, value_parser = parse_ttl, value_name = "DURATION")]
        ttl: Option<u64>,
        #[arg(long)]
        wait: bool,
    },
    /// Release a lease (the holder's, or any as the user)
    Release { name: String },
    /// List leases
    Ls {
        /// Print a JSON array
        #[arg(long)]
        json: bool,
    },
}

/// `90`, `90s`, `30m`, `1h`, `1h30m`, `2d`: whole seconds, at least one.
pub fn parse_ttl(s: &str) -> std::result::Result<u64, String> {
    let bad = || format!("{s:?} is not a duration like 90s, 30m, 1h or 1h30m");
    // (digits, unit) groups; a bare number is seconds, but only on its own.
    let mut groups: Vec<(u64, u64)> = Vec::new();
    let mut digits = String::new();
    for c in s.chars() {
        let unit = match c {
            '0'..='9' => {
                digits.push(c);
                continue;
            }
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => return Err(bad()),
        };
        groups.push((digits.parse().map_err(|_| bad())?, unit));
        digits.clear();
    }
    if !digits.is_empty() {
        if !groups.is_empty() {
            return Err(bad());
        }
        groups.push((digits.parse().map_err(|_| bad())?, 1));
    }
    let mut total: u64 = 0;
    for (n, unit) in groups {
        total = n
            .checked_mul(unit)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(bad)?;
    }
    if total == 0 { Err(bad()) } else { Ok(total) }
}

/// A hold key: random, so another client's cannot collide with it.
fn new_key() -> i64 {
    let mut b = [0u8; 8];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    let n = match read {
        Ok(()) => u64::from_le_bytes(b),
        Err(_) => {
            u64::from(std::process::id())
                ^ std::time::UNIX_EPOCH
                    .elapsed()
                    .map_or(0, |d| d.as_nanos() as u64)
        }
    };
    i64::try_from(n >> 1).unwrap_or(1).max(1)
}

/// What an error of the server's means for the exit code: busy and not
/// held are 1, forbidden is 3 (by `main`), the rest 2.
fn exit_for(e: &Error) -> Option<u8> {
    match e {
        Error::Server(m) if m.starts_with("busy:") || m.starts_with("not held:") => Some(1),
        _ if e.is_forbidden() => None,
        _ => Some(2),
    }
}

/// Runs the command: the process's exit code, or the error `main` reports
/// (and exits 3 for when forbidden).
pub fn run(client: Connection, sock: &Path, token: Option<&str>, args: Args) -> Result<u8> {
    let outcome = match args.cmd {
        Some(Sub::Acquire {
            name,
            max_load,
            ttl,
            wait,
        }) => acquire(client, &name, ttl, max_load, wait),
        Some(Sub::Release { name }) => release(client, &name),
        Some(Sub::Ls { json }) => list(client, json),
        None => {
            let name = args.name.unwrap_or_default();
            hold(
                client,
                sock,
                token,
                Hold {
                    name,
                    ttl: args.ttl,
                    max_load: args.max_load,
                    wait: args.wait,
                    command: args.command,
                },
            )
        }
    };
    match outcome {
        Ok(code) => Ok(code),
        Err(e) => match exit_for(&e) {
            Some(code) => {
                eprintln!("fleet: {e}");
                Ok(code)
            }
            None => Err(e),
        },
    }
}

fn request_for(
    name: &str,
    key: i64,
    ttl: Option<u64>,
    max_load: Option<f64>,
    wait: bool,
    hold: bool,
) -> AcquireLease {
    AcquireLease {
        name: name.to_owned(),
        holder_key: key,
        ttl_secs: ttl.unwrap_or(0),
        max_load,
        wait,
        hold,
    }
}

fn granted(r: response::Kind) -> Result<LeaseInfo> {
    match r {
        response::Kind::LeaseGranted(g) => g
            .lease
            .ok_or_else(|| Error::Protocol("empty lease grant".into())),
        _ => Err(Error::Protocol("unexpected reply to acquire".into())),
    }
}

fn acquire(
    mut client: Connection,
    name: &str,
    ttl: Option<u64>,
    max_load: Option<f64>,
    wait: bool,
) -> Result<u8> {
    let req = request_for(name, new_key(), ttl, max_load, wait, false);
    let done = notice_if_slow(name, wait);
    let reply = client.call(request::Kind::AcquireLease(req));
    done.store(true, Ordering::SeqCst);
    let info = granted(reply?)?;
    match info.expires_unix_ms {
        Some(t) => println!("acquired {name} (expires in {})", until(t)),
        None => println!("acquired {name}"),
    }
    Ok(0)
}

fn release(mut client: Connection, name: &str) -> Result<u8> {
    match client.call(request::Kind::ReleaseLease(ReleaseLease {
        name: name.to_owned(),
    }))? {
        response::Kind::Ack(_) => {
            println!("released {name}");
            Ok(0)
        }
        _ => Err(Error::Protocol("unexpected reply to release".into())),
    }
}

/// Prints "waiting for lease …" once, if the answer takes a while.
fn notice_if_slow(name: &str, wait: bool) -> Arc<AtomicBool> {
    let done = Arc::new(AtomicBool::new(false));
    if wait {
        let (flag, name) = (Arc::clone(&done), name.to_owned());
        thread::spawn(move || {
            thread::sleep(WAITING_NOTICE_AFTER);
            if !flag.load(Ordering::SeqCst) {
                eprintln!("fleet: waiting for lease {name:?}");
            }
        });
    }
    done
}

struct Hold {
    name: String,
    ttl: Option<u64>,
    max_load: Option<f64>,
    wait: bool,
    command: Vec<String>,
}

/// Connects, retrying while a daemon restart is under way.
fn reconnect(sock: &Path, token: Option<&str>) -> Result<Connection> {
    let until = Instant::now() + RECONNECT_FOR;
    loop {
        match Connection::unix_as(sock, "fleet", token) {
            Ok(c) => return Ok(c),
            Err(e @ (Error::NotRunning | Error::Io(_) | Error::Timeout))
                if Instant::now() < until =>
            {
                let _ = e;
                thread::sleep(RECONNECT_EVERY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Whether `e` is the connection going away (a daemon restart or stop),
/// as opposed to an answer.
fn connection_lost(e: &Error) -> bool {
    matches!(e, Error::Io(_) | Error::Protocol(_) | Error::NotRunning)
}

fn hold(mut client: Connection, sock: &Path, token: Option<&str>, h: Hold) -> Result<u8> {
    let key = new_key();
    let req = request_for(&h.name, key, h.ttl, h.max_load, h.wait, true);
    let done = notice_if_slow(&h.name, h.wait);
    // Asked again on a fresh connection if the daemon restarts meanwhile:
    // the same key reclaims a lease it already granted, and otherwise the
    // request queues again (at the back: the order is not kept).
    let until = Instant::now() + RECONNECT_FOR;
    let conn = loop {
        match client.call(request::Kind::AcquireLease(req.clone())) {
            Ok(r) => {
                granted(r)?;
                break client;
            }
            Err(e) if connection_lost(&e) && Instant::now() < until => {
                client = reconnect(sock, token)?;
            }
            Err(e) => {
                done.store(true, Ordering::SeqCst);
                return Err(e);
            }
        }
    };
    done.store(true, Ordering::SeqCst);

    // The lease is ours while this process lives and `conn` stays open.
    let watcher = watch(conn, sock.to_owned(), token.map(str::to_owned), req);

    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT])?;
    let mut child = match Command::new(&h.command[0]).args(&h.command[1..]).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fleet: {}: {e}", h.command[0]);
            // The shell's convention: not found 127, not executable 126.
            return Ok(if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            });
        }
    };
    // A terminal sends SIGINT and SIGQUIT to the command too; fleet only
    // has to survive them, or the lease would go while the command runs.
    // SIGTERM and SIGHUP are sent to fleet alone, so they are passed on.
    let pid = child.id() as libc::pid_t;
    thread::spawn(move || {
        for sig in signals.forever() {
            if sig == SIGTERM || sig == SIGHUP {
                // SAFETY: kill has no memory-safety preconditions.
                unsafe { libc::kill(pid, sig) };
            }
        }
    });
    let status = child.wait()?;
    drop(watcher);
    Ok(match (status.code(), status.signal()) {
        (Some(c), _) => c as u8,
        (None, Some(s)) => 128 + s as u8,
        (None, None) => 1,
    })
}

/// Keeps the lease's connection, and gets it back after a daemon restart:
/// while the command runs, a reader notices the connection ending, then
/// reconnects and reclaims the lease with its key. Dropped at the end.
struct Watcher {
    _stop: mpsc::Sender<()>,
}

fn watch(
    conn: Connection,
    sock: std::path::PathBuf,
    token: Option<String>,
    req: AcquireLease,
) -> Watcher {
    let (stop, stopped) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut conn = conn;
        loop {
            let (mut reader, _sender) = conn.split();
            // Frames on this connection are events we do not use.
            while let Ok(Some(_)) = reader.read_frame() {}
            if stopped.try_recv() != Err(mpsc::TryRecvError::Empty) {
                return;
            }
            // Reclaim: not waiting (the lease is ours or it is lost).
            let mut again = req.clone();
            again.wait = false;
            let reclaimed = reconnect(&sock, token.as_deref()).and_then(|mut c| {
                granted(c.call(request::Kind::AcquireLease(again)).map_err(|e| e)?)?;
                Ok(c)
            });
            match reclaimed {
                Ok(c) => conn = c,
                Err(e) => {
                    eprintln!("fleet: lost lease {:?}: {e}", req.name);
                    return;
                }
            }
        }
    });
    Watcher { _stop: stop }
}

fn list(mut client: Connection, as_json: bool) -> Result<u8> {
    let leases = match client.call(request::Kind::ListLeases(ListLeases {}))? {
        response::Kind::Leases(l) => l.leases,
        _ => return Err(Error::Protocol("unexpected reply to lease ls".into())),
    };
    let now = now_ms();
    if as_json {
        let items: Vec<Value> = leases.iter().map(json_lease).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&items).unwrap_or_default()
        );
    } else {
        for line in table(&leases, now) {
            println!("{line}");
        }
    }
    Ok(0)
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// "in 5m" until `unix_ms` from now (for the acquire message).
fn until(unix_ms: i64) -> String {
    span(((unix_ms - now_ms()) / 1000).max(0))
}

/// 90 -> "1m", 5400 -> "1h30m": at most two units.
fn span(secs: i64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        secs % 86400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m"),
        (0, _, 0) => format!("{h}h"),
        (0, _, _) => format!("{h}h{m}m"),
        (_, 0, _) => format!("{d}d"),
        _ => format!("{d}d{h}h"),
    }
}

fn holder_text(l: &LeaseInfo) -> String {
    match (l.held, l.holder, l.holder_hidden) {
        (false, _, _) => "-".into(),
        (_, _, true) => "another agent".into(),
        (_, Some(a), _) => format!("agent {a}"),
        (_, None, _) => "user".into(),
    }
}

fn table(leases: &[LeaseInfo], now: i64) -> Vec<String> {
    if leases.is_empty() {
        return vec!["no leases".into()];
    }
    let mut rows = vec![[
        "NAME".to_owned(),
        "HOLDER".into(),
        "HELD".into(),
        "EXPIRES".into(),
        "WAITING".into(),
    ]];
    for l in leases {
        rows.push([
            l.name.clone(),
            holder_text(l),
            if l.held {
                span(((now - l.acquired_unix_ms) / 1000).max(0))
            } else {
                "-".into()
            },
            match l.expires_unix_ms {
                Some(t) => format!("in {}", span(((t - now) / 1000).max(0))),
                None if l.held_by_connection => "with its command".into(),
                None => "-".into(),
            },
            l.waiters.to_string(),
        ]);
    }
    let widths: Vec<usize> = (0..5)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|r| {
            let cells: Vec<String> = r
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
                .collect();
            cells.join("  ").trim_end().to_owned()
        })
        .collect()
}

fn json_lease(l: &LeaseInfo) -> Value {
    let kind = match (l.held, l.holder, l.holder_hidden) {
        (false, _, _) => Value::Null,
        (_, _, true) => json!("other"),
        (_, Some(_), _) => json!("agent"),
        (_, None, _) => json!("user"),
    };
    json!({
        "name": l.name,
        "held": l.held,
        "holder_kind": kind,
        "holder": l.holder,
        "acquired_unix_ms": l.held.then_some(l.acquired_unix_ms),
        "expires_unix_ms": l.expires_unix_ms,
        "held_by_connection": l.held_by_connection,
        "waiters": l.waiters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_parses_units_and_compounds() {
        assert_eq!(parse_ttl("90"), Ok(90));
        assert_eq!(parse_ttl("90s"), Ok(90));
        assert_eq!(parse_ttl("30m"), Ok(1800));
        assert_eq!(parse_ttl("1h"), Ok(3600));
        assert_eq!(parse_ttl("1h30m"), Ok(5400));
        assert_eq!(parse_ttl("2d"), Ok(172_800));
    }

    #[test]
    fn ttl_rejects_nonsense() {
        for bad in [
            "",
            "0",
            "0s",
            "h",
            "1x",
            "-5",
            "1h30",
            "1.5h",
            "1 h",
            "99999999999999999999",
        ] {
            assert!(parse_ttl(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn spans_show_two_units_at_most() {
        assert_eq!(span(5), "5s");
        assert_eq!(span(90), "1m");
        assert_eq!(span(3600), "1h");
        assert_eq!(span(5400), "1h30m");
        assert_eq!(span(90_000), "1d1h");
    }

    fn lease(name: &str, holder: Option<u64>) -> LeaseInfo {
        LeaseInfo {
            name: name.into(),
            held: true,
            holder,
            acquired_unix_ms: 0,
            expires_unix_ms: Some(120_000),
            held_by_connection: false,
            waiters: 2,
            ..Default::default()
        }
    }

    #[test]
    fn the_table_names_holders_and_lines_up() {
        let mut hidden = lease("c", Some(9));
        hidden.holder_hidden = true;
        hidden.holder = None;
        let t = table(
            &[lease("perf-wall", Some(4)), lease("b", None), hidden],
            60_000,
        );
        assert_eq!(t[0], "NAME       HOLDER         HELD  EXPIRES  WAITING");
        assert_eq!(t[1], "perf-wall  agent 4        1m    in 1m    2");
        assert!(t[2].starts_with("b          user"));
        assert!(t[3].contains("another agent"));
        assert_eq!(table(&[], 0), ["no leases"]);
    }

    #[test]
    fn json_names_the_kind_of_holder() {
        assert_eq!(json_lease(&lease("x", Some(3)))["holder_kind"], "agent");
        assert_eq!(json_lease(&lease("x", None))["holder_kind"], "user");
        assert_eq!(json_lease(&lease("x", Some(3)))["holder"], 3);
    }
}
