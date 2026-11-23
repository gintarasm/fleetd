//! `fleet node exec|cat|ls` (ADR 0091): the node's primitives, for the user
//! (an agent is refused, exit 3). They exist so the primitives can be tested
//! and scripted against a remote node later (M6).

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args as ClapArgs, Subcommand};
use fleet_client::node::{exec, list_dir, read_file};
use fleet_client::{Connection, Error, Result};
use fleet_proto::{EntryKind, Exec, ExecResult, ListDir, ReadFile};
use serde_json::json;

use crate::mailbox::parse_duration;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a program on the node and print its output (exits with its code,
    /// 124 on a timeout)
    Exec {
        /// Working directory on the node (default: this shell's)
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// Kill it after this long (e.g. 30s, 5m; default 10m, at most 1h)
        #[arg(long, value_parser = parse_duration, value_name = "DURATION")]
        timeout: Option<Duration>,
        /// Add to the node's login-shell environment (repeatable)
        #[arg(long = "env", value_name = "K=V")]
        env: Vec<String>,
        /// The program and its arguments
        #[arg(last = true, required = true, value_name = "ARGV")]
        argv: Vec<String>,
    },
    /// Print a file on the node
    Cat {
        /// Absolute path
        path: PathBuf,
    },
    /// List a directory on the node
    Ls {
        /// Absolute path
        dir: PathBuf,
        /// Print a JSON object (`entries`, `truncated`)
        #[arg(long)]
        json: bool,
    },
}

pub fn run(client: &mut Connection, args: &Args) -> Result<u8> {
    match &args.command {
        Command::Exec {
            cwd,
            timeout,
            env,
            argv,
        } => run_exec(client, cwd.as_deref(), *timeout, env, argv),
        Command::Cat { path } => cat(client, path),
        Command::Ls { dir, json } => ls(client, dir, *json),
    }
}

/// An absolute form of `p`, relative to this shell's directory.
fn absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn run_exec(
    client: &mut Connection,
    cwd: Option<&Path>,
    timeout: Option<Duration>,
    env: &[String],
    argv: &[String],
) -> Result<u8> {
    let req = Exec {
        argv: argv.to_vec(),
        cwd: absolute(cwd.unwrap_or(Path::new(".")))?
            .to_string_lossy()
            .into_owned(),
        env: parse_env(env)?,
        timeout_ms: timeout.map_or(0, |t| t.as_millis().min(u128::from(u32::MAX)) as u32),
        ..Default::default()
    };
    let r = exec(client, req)?;
    // A closed stdout or stderr (piped into `head`) is not an error here.
    let _ = io::stdout().lock().write_all(&r.stdout);
    let _ = io::stderr().lock().write_all(&r.stderr);
    for (cut, stream) in [
        (r.stdout_truncated, "stdout"),
        (r.stderr_truncated, "stderr"),
    ] {
        if cut {
            eprintln!("fleet: {stream} cut at the node's output cap (15 MiB in all)");
        }
    }
    Ok(exit_code(&r))
}

/// `timeout(1)`'s 124 on a timeout, the shell's `128 + signal` for a child
/// a signal ended, else the child's own code.
fn exit_code(r: &ExecResult) -> u8 {
    if r.timed_out {
        124
    } else if r.signal != 0 {
        (128 + r.signal) as u8
    } else {
        r.exit_code as u8
    }
}

fn parse_env(pairs: &[String]) -> Result<HashMap<String, String>> {
    pairs
        .iter()
        .map(|p| match p.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
            _ => Err(Error::Server(format!("invalid --env {p:?}: expected K=V"))),
        })
        .collect()
}

fn cat(client: &mut Connection, path: &Path) -> Result<u8> {
    let path = absolute(path)?.to_string_lossy().into_owned();
    let mut out = io::stdout().lock();
    let mut offset = 0;
    loop {
        let d = read_file(
            client,
            ReadFile {
                path: path.clone(),
                offset,
                max_bytes: 0,
            },
        )?;
        if out.write_all(&d.data).is_err() {
            // stdout closed (piped into `head`).
            return Ok(0);
        }
        offset += d.data.len() as u64;
        if d.eof || d.data.is_empty() {
            break;
        }
    }
    out.flush()?;
    Ok(0)
}

fn kind_name(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Symlink => "symlink",
        EntryKind::Other | EntryKind::Unspecified => "other",
    }
}

fn ls(client: &mut Connection, dir: &Path, as_json: bool) -> Result<u8> {
    let dir = absolute(dir)?.to_string_lossy().into_owned();
    let mut listing = list_dir(
        client,
        ListDir {
            path: dir,
            max_entries: 0,
        },
    )?;
    listing.entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = io::stdout().lock();
    if as_json {
        let entries: Vec<_> = listing
            .entries
            .iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "kind": kind_name(e.kind()),
                    "size": e.size,
                    "mtime_unix_ms": e.mtime_unix_ms,
                })
            })
            .collect();
        writeln!(
            out,
            "{}",
            json!({ "entries": entries, "truncated": listing.truncated })
        )?;
    } else {
        for e in &listing.entries {
            let slash = if e.kind() == EntryKind::Dir { "/" } else { "" };
            if writeln!(
                out,
                "{:<8}{:>12}  {}{slash}",
                kind_name(e.kind()),
                e.size,
                e.name
            )
            .is_err()
            {
                return Ok(0);
            }
        }
    }
    if listing.truncated {
        eprintln!("fleet: more entries not shown (the node lists at most 10,000)");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(code: i32, signal: i32, timed_out: bool) -> ExecResult {
        ExecResult {
            exit_code: code,
            signal,
            timed_out,
            ..Default::default()
        }
    }

    #[test]
    fn exit_codes_follow_timeout_1_and_the_shell() {
        assert_eq!(exit_code(&result(0, 0, false)), 0);
        assert_eq!(exit_code(&result(7, 0, false)), 7);
        assert_eq!(exit_code(&result(-1, 15, true)), 124);
        assert_eq!(exit_code(&result(-1, 9, false)), 137);
    }

    #[test]
    fn env_pairs_need_a_key() {
        let got = parse_env(&["A=1".into(), "B=x=y".into(), "C=".into()]).unwrap();
        assert_eq!(got["B"], "x=y");
        assert_eq!(got["C"], "");
        assert!(parse_env(&["noequals".into()]).is_err());
        assert!(parse_env(&["=v".into()]).is_err());
    }
}
