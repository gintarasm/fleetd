//! `SearchOutput` (ADR 0098): every agent's output searched for a text,
//! for `fleet search` and the GUI's Search All Agents.

use fleet_proto::{SearchHits, SearchOutput, request, response};

use crate::{Connection, Error, Result};

/// Asks the node to search every agent's screen and history.
pub fn search(conn: &mut Connection, req: SearchOutput) -> Result<SearchHits> {
    match conn.call(request::Kind::SearchOutput(req))? {
        response::Kind::SearchHits(hits) => Ok(hits),
        _ => Err(Error::Protocol("unexpected reply to search".into())),
    }
}
