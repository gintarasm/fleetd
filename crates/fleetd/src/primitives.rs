//! The node primitives over the wire (ADR 0091): proto in, [`fleet_node::primitives`]
//! calls, proto out. `Exec` runs on its own task (see
//! `Connection::start_exec`); `ReadFile` and `ListDir` are short and run on
//! the blocking pool like `ReadHistory`.

use std::path::PathBuf;
use std::time::Duration;

use fleet_node::env::LoginEnv;
use fleet_node::primitives as p;
use fleet_proto::{
    DirEntry, DirListing, EntryKind, Exec, ExecResult, FileData, Frame, ListDir, ReadFile,
    Response, response,
};
use tokio::sync::mpsc;

/// Execs one connection may have running or queued: each is a task, and a
/// client that never reads its answers must not pile them up.
pub(crate) const MAX_EXECS_PER_CONNECTION: usize = 32;

fn error(e: std::io::Error) -> response::Kind {
    response::Kind::Error(fleet_proto::Error {
        message: e.to_string(),
    })
}

/// Runs `req` and queues the answer to request `id`.
pub(crate) async fn exec(env: LoginEnv, id: u64, req: Exec, outbox: mpsc::Sender<Frame>) {
    let req = p::ExecRequest {
        argv: req.argv,
        cwd: PathBuf::from(req.cwd),
        env: req.env.into_iter().collect(),
        clear_env: req.clear_env,
        stdin: req.stdin,
        timeout: Duration::from_millis(u64::from(req.timeout_ms)),
        max_output: req.max_output as usize,
    };
    let kind = match p::exec(req, env).await {
        Ok(r) => response::Kind::ExecResult(Box::new(ExecResult {
            exit_code: r.exit_code,
            signal: r.signal,
            timed_out: r.timed_out,
            stdout: r.stdout,
            stderr: r.stderr,
            stdout_truncated: r.stdout_truncated,
            stderr_truncated: r.stderr_truncated,
            duration_ms: r.duration.as_millis() as u64,
        })),
        Err(e) => error(e),
    };
    let _ = outbox
        .send(Frame::Response(Response {
            id,
            kind: Some(kind),
        }))
        .await;
}

pub(crate) async fn read_file(r: ReadFile) -> response::Kind {
    let req = p::ReadFileRequest {
        path: PathBuf::from(r.path),
        offset: r.offset,
        max_bytes: r.max_bytes as usize,
    };
    match tokio::task::spawn_blocking(move || p::read_file(&req)).await {
        Ok(Ok(d)) => response::Kind::FileData(Box::new(FileData {
            data: d.data,
            size: d.size,
            mtime_unix_ms: d.mtime_unix_ms,
            eof: d.eof,
        })),
        Ok(Err(e)) => error(e),
        Err(e) => error(std::io::Error::other(e)),
    }
}

pub(crate) async fn list_dir(r: ListDir) -> response::Kind {
    let req = p::ListDirRequest {
        path: PathBuf::from(r.path),
        max_entries: r.max_entries as usize,
    };
    match tokio::task::spawn_blocking(move || p::list_dir(&req)).await {
        Ok(Ok(l)) => response::Kind::DirListing(DirListing {
            entries: l
                .entries
                .into_iter()
                .map(|e| DirEntry {
                    name: e.name,
                    kind: match e.kind {
                        p::EntryKind::File => EntryKind::File,
                        p::EntryKind::Dir => EntryKind::Dir,
                        p::EntryKind::Symlink => EntryKind::Symlink,
                        p::EntryKind::Other => EntryKind::Other,
                    }
                    .into(),
                    size: e.size,
                    mtime_unix_ms: e.mtime_unix_ms,
                })
                .collect(),
            truncated: l.truncated,
        }),
        Ok(Err(e)) => error(e),
        Err(e) => error(std::io::Error::other(e)),
    }
}
