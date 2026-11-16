//! `fleet history` and `fleet screen`: read-only views built from an attach
//! snapshot plus `ReadHistory` pages ([`fleet_client::history`]).

use std::io::{self, Write};

use fleet_client::{Connection, Result, history as h};

pub fn screen(mut client: Connection, agent_id: u64) -> Result<()> {
    let rows = h::screen(&mut client, agent_id)?;
    let mut out = io::stdout().lock();
    for row in rows {
        writeln!(out, "{row}")?;
    }
    Ok(())
}

pub fn history(
    mut client: Connection,
    agent_id: u64,
    limit: Option<usize>,
    plain: bool,
) -> Result<()> {
    let t = h::read(&mut client, agent_id, limit)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    if plain {
        for line in t.plain_lines() {
            writeln!(out, "{line}")?;
        }
    } else {
        t.write_ansi(&mut out)?;
    }
    out.flush()?;
    Ok(())
}
