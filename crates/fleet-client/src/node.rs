//! The node primitives (ADR 0091): `Exec`, `ReadFile` and `ListDir`, for
//! `fleet node` and tests. The user's only: an agent's call is refused.

use fleet_proto::{DirListing, Exec, ExecResult, FileData, ListDir, ReadFile, request, response};

use crate::{Connection, Error, Result};

/// Runs a program on the node and waits for it (up to the request's timeout).
pub fn exec(conn: &mut Connection, req: Exec) -> Result<ExecResult> {
    match conn.call(request::Kind::Exec(Box::new(req)))? {
        response::Kind::ExecResult(r) => Ok(*r),
        _ => Err(Error::Protocol("unexpected reply to exec".into())),
    }
}

/// Reads a range of a file on the node.
pub fn read_file(conn: &mut Connection, req: ReadFile) -> Result<FileData> {
    match conn.call(request::Kind::ReadFile(req))? {
        response::Kind::FileData(d) => Ok(*d),
        _ => Err(Error::Protocol("unexpected reply to read_file".into())),
    }
}

/// Lists a directory on the node.
pub fn list_dir(conn: &mut Connection, req: ListDir) -> Result<DirListing> {
    match conn.call(request::Kind::ListDir(req))? {
        response::Kind::DirListing(l) => Ok(l),
        _ => Err(Error::Protocol("unexpected reply to list_dir".into())),
    }
}
