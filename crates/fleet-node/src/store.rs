//! SQLite agent store (decision 0010): one WAL-mode DB per node.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};

/// v6 and v7 are taken by the M3 mailbox and lease branches (spec
/// "Waves"); identity took 8.
const SCHEMA_VERSION: i64 = 8;

/// An agent's labels (ADR 0025); the node validates them.
pub type Labels = HashMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Running,
    Exited,
    Lost,
}

#[derive(Debug, Clone)]
pub struct NewAgent {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub created_unix_ms: i64,
    pub scrollback_path: String,
    pub labels: Labels,
    /// The `AgentPriority` it was spawned with (ADR 0066), as its proto
    /// value.
    pub priority: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub id: u64,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub state: AgentState,
    pub exit_code: Option<i32>,
    pub created_unix_ms: i64,
    pub scrollback_path: String,
    pub labels: Labels,
    pub priority: i32,
    /// When the agent exited; `None` while running, for a lost agent, and
    /// for an exit recorded before schema v5.
    pub exited_unix_ms: Option<i64>,
}

pub struct Store {
    conn: Connection,
}

/// An agent's token digest (ADR 0072): only while it runs.
const AGENT_TOKENS_DDL: &str = "CREATE TABLE agent_tokens (
                 agent_id INTEGER PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
                 digest BLOB NOT NULL UNIQUE
             );";

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            AgentState::Running => "running",
            AgentState::Exited => "exited",
            AgentState::Lost => "lost",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(AgentState::Running),
            "exited" => Some(AgentState::Exited),
            "lost" => Some(AgentState::Lost),
            _ => None,
        }
    }
}

fn conversion_err(col: usize, e: Box<dyn std::error::Error + Send + Sync>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, e)
}

const AGENT_COLUMNS: &str = "SELECT id, program, args_json, cwd, state, exit_code, \
     created_unix_ms, scrollback_path, priority, exited_unix_ms FROM agents";

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Decodes an `AGENT_COLUMNS` row, taking its labels out of `labels`.
fn decode_agent(
    r: &rusqlite::Row<'_>,
    labels: &mut HashMap<u64, Labels>,
) -> rusqlite::Result<AgentRecord> {
    let args_json: String = r.get(2)?;
    let state: String = r.get(4)?;
    let id = r.get::<_, i64>(0)? as u64;
    Ok(AgentRecord {
        id,
        program: r.get(1)?,
        args: serde_json::from_str(&args_json).map_err(|e| conversion_err(2, Box::new(e)))?,
        cwd: r.get(3)?,
        state: AgentState::parse(&state)
            .ok_or_else(|| conversion_err(4, format!("unknown state {state:?}").into()))?,
        exit_code: r.get(5)?,
        created_unix_ms: r.get(6)?,
        scrollback_path: r.get(7)?,
        labels: labels.remove(&id).unwrap_or_default(),
        priority: r.get(8)?,
        exited_unix_ms: r.get(9)?,
    })
}

impl Store {
    /// Opens (creating with mode 0600 if new) the DB, enables WAL and
    /// migrates to the latest schema. Fails on a DB written by a newer schema.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if !path.exists() {
            use std::os::unix::fs::OpenOptionsExt;
            let created = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path);
            match created {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(e))),
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_SCHEMA),
                Some(format!(
                    "database schema version {version} is newer than supported {SCHEMA_VERSION}"
                )),
            ));
        }
        if version == 0 {
            // A new DB gets the latest schema at once: replaying v1 -> v2 ->
            // v3 (a table rebuild) leaves ~1 MB of SQLite heap behind, which
            // shows in the daemon's idle RSS.
            return self.create_latest();
        }
        // An existing DB is at v1 or later here (v1 created the `agents`
        // table); it migrates in place.
        if version < 2 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE agent_labels (
                     agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
                     key TEXT NOT NULL,
                     value TEXT NOT NULL,
                     PRIMARY KEY (agent_id, key)
                 );
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        }
        if version < 3 {
            self.migrate_to_v3()?;
        }
        if version < 4 {
            // v4: the agent's CPU priority (ADR 0066). The column DDL must
            // match `create_latest`'s (the schema-equality test compares them).
            self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE agents ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
                 PRAGMA user_version = 4;
                 COMMIT;",
            )?;
        }
        if version < 5 {
            // v5: when an exited agent exited (ADR 0058). Old rows keep NULL.
            self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE agents ADD COLUMN exited_unix_ms INTEGER;
                 PRAGMA user_version = 5;
                 COMMIT;",
            )?;
        }
        if version < 8 {
            // v8: agent token digests (ADR 0072). The DDL must match
            // `create_latest`'s.
            self.conn.execute_batch(&format!(
                "BEGIN;
                 {AGENT_TOKENS_DDL}
                 PRAGMA user_version = 8;
                 COMMIT;"
            ))?;
        }
        Ok(())
    }

    /// The latest schema for a new DB; the same tables the v1 -> ... -> v8
    /// migrations produce.
    fn create_latest(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(&format!(
            "BEGIN;
             CREATE TABLE agents (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 program TEXT NOT NULL,
                 args_json TEXT NOT NULL,
                 cwd TEXT NOT NULL,
                 state TEXT NOT NULL CHECK(state IN ('running','exited','lost')),
                 exit_code INTEGER,
                 created_unix_ms INTEGER NOT NULL,
                 scrollback_path TEXT NOT NULL, priority INTEGER NOT NULL DEFAULT 0, exited_unix_ms INTEGER
             );
             CREATE TABLE agent_labels (
                 agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 PRIMARY KEY (agent_id, key)
             );
             CREATE TABLE agent_replacements (
                 old_id INTEGER PRIMARY KEY,
                 new_id INTEGER NOT NULL
             );
             {AGENT_TOKENS_DDL}
             PRAGMA user_version = 8;
             COMMIT;"
        ))
    }

    /// v3: agent ids use AUTOINCREMENT so they are never reused, and
    /// `agent_replacements` records restarted agents' successors. The table
    /// rebuild needs foreign keys off (a no-op inside a transaction) so the
    /// DROP does not cascade into `agent_labels`.
    fn migrate_to_v3(&self) -> rusqlite::Result<()> {
        self.conn.pragma_update(None, "foreign_keys", "OFF")?;
        let result = self.rebuild_agents_v3();
        let restore = self.conn.pragma_update(None, "foreign_keys", "ON");
        result?;
        restore
    }

    /// The `agents_v3` and `agent_replacements` DDL here must stay in sync
    /// with `create_latest` before its v4 and v5 columns (the schema-equality test
    /// compares the two).
    fn rebuild_agents_v3(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN")?;
        let run = || -> rusqlite::Result<()> {
            self.conn.execute_batch(
                "CREATE TABLE agents_v3 (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     program TEXT NOT NULL,
                     args_json TEXT NOT NULL,
                     cwd TEXT NOT NULL,
                     state TEXT NOT NULL CHECK(state IN ('running','exited','lost')),
                     exit_code INTEGER,
                     created_unix_ms INTEGER NOT NULL,
                     scrollback_path TEXT NOT NULL
                 );
                 INSERT INTO agents_v3 SELECT * FROM agents;
                 DROP TABLE agents;
                 ALTER TABLE agents_v3 RENAME TO agents;
                 CREATE TABLE agent_replacements (
                     old_id INTEGER PRIMARY KEY,
                     new_id INTEGER NOT NULL
                 );",
            )?;
            let violations: i64 =
                self.conn
                    .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                        r.get(0)
                    })?;
            if violations > 0 {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                    Some("v3 migration left foreign key violations".into()),
                ));
            }
            self.conn.execute_batch("PRAGMA user_version = 3")
        };
        // A failed COMMIT (e.g. SQLITE_BUSY) can leave the transaction open;
        // roll it back like any other error so the connection is usable.
        match run().and_then(|()| self.conn.execute_batch("COMMIT")) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Inserts a running agent and its labels, in one transaction.
    pub fn insert_agent(&self, rec: &NewAgent) -> rusqlite::Result<u64> {
        let tx = self.conn.unchecked_transaction()?;
        let id = self.insert_agent_row(rec)?;
        tx.commit()?;
        Ok(id)
    }

    fn insert_agent_row(&self, rec: &NewAgent) -> rusqlite::Result<u64> {
        let args_json = serde_json::to_string(&rec.args)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.conn.execute(
            "INSERT INTO agents (program, args_json, cwd, state, created_unix_ms, scrollback_path, priority)
             VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6)",
            (
                &rec.program,
                args_json,
                &rec.cwd,
                rec.created_unix_ms,
                &rec.scrollback_path,
                rec.priority,
            ),
        )?;
        let id = self.conn.last_insert_rowid() as u64;
        self.insert_labels(id, &rec.labels)?;
        Ok(id)
    }

    fn insert_labels(&self, id: u64, labels: &Labels) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO agent_labels (agent_id, key, value) VALUES (?1, ?2, ?3)",
        )?;
        for (key, value) in labels {
            stmt.execute((id as i64, key, value))?;
        }
        Ok(())
    }

    /// The agent's labels; `None` if the agent is unknown.
    pub fn labels(&self, id: u64) -> rusqlite::Result<Option<Labels>> {
        let known = self
            .conn
            .query_row("SELECT 1 FROM agents WHERE id = ?1", (id as i64,), |_| {
                Ok(())
            })
            .optional()?;
        if known.is_none() {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare_cached("SELECT key, value FROM agent_labels WHERE agent_id = ?1")?;
        let labels = stmt
            .query_map((id as i64,), |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Labels>>()?;
        Ok(Some(labels))
    }

    /// Replaces the agent's labels with `labels`, in one transaction.
    pub fn replace_labels(&self, id: u64, labels: &Labels) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.conn
            .execute("DELETE FROM agent_labels WHERE agent_id = ?1", (id as i64,))?;
        self.insert_labels(id, labels)?;
        tx.commit()
    }

    /// Inserts a running agent whose scrollback path is derived from its id
    /// (`rec.scrollback_path` is replaced by `scrollback_path(id)`), in one
    /// transaction.
    pub fn insert_agent_with_scrollback(
        &self,
        rec: &NewAgent,
        scrollback_path: impl FnOnce(u64) -> String,
    ) -> rusqlite::Result<(u64, String)> {
        let tx = self.conn.unchecked_transaction()?;
        let id = self.insert_agent_row(rec)?;
        let path = scrollback_path(id);
        self.conn.execute(
            "UPDATE agents SET scrollback_path = ?2 WHERE id = ?1",
            (id as i64, &path),
        )?;
        tx.commit()?;
        Ok((id, path))
    }

    /// Removes an agent's row (e.g. after its spawn failed).
    pub fn delete_agent(&self, id: u64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM agents WHERE id = ?1", (id as i64,))?;
        Ok(())
    }

    /// Deletes an agent that is not running (labels cascade) and any
    /// replacement records pointing at it. `Ok(false)` if it is unknown or
    /// running; nothing is deleted then.
    pub fn forget_agent(&self, id: u64) -> rusqlite::Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let n = self.conn.execute(
            "DELETE FROM agents WHERE id = ?1 AND state != 'running'",
            (id as i64,),
        )?;
        if n == 1 {
            self.conn.execute(
                "DELETE FROM agent_replacements WHERE new_id = ?1",
                (id as i64,),
            )?;
        }
        tx.commit()?;
        Ok(n == 1)
    }

    /// Records that `new` replaced `old`: deletes `old`, re-points anything
    /// that pointed at `old` to `new` (chains collapse), in one transaction.
    pub fn replace_agent(&self, old: u64, new: u64) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.conn
            .execute("DELETE FROM agents WHERE id = ?1", (old as i64,))?;
        self.conn.execute(
            "UPDATE agent_replacements SET new_id = ?2 WHERE new_id = ?1",
            (old as i64, new as i64),
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO agent_replacements (old_id, new_id) VALUES (?1, ?2)",
            (old as i64, new as i64),
        )?;
        tx.commit()
    }

    /// Old agent id -> the agent that replaced it.
    pub fn replacements(&self) -> rusqlite::Result<HashMap<u64, u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT old_id, new_id FROM agent_replacements")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })?;
        rows.collect()
    }

    /// One agent with its labels; `None` if unknown.
    pub fn agent(&self, id: u64) -> rusqlite::Result<Option<AgentRecord>> {
        let rec = self
            .conn
            .query_row(
                &format!("{AGENT_COLUMNS} WHERE id = ?1"),
                (id as i64,),
                |r| decode_agent(r, &mut HashMap::new()),
            )
            .optional()?;
        let Some(mut rec) = rec else {
            return Ok(None);
        };
        rec.labels = self.labels(id)?.unwrap_or_default();
        Ok(Some(rec))
    }

    /// Stores the digest of a running agent's token, replacing any it had.
    pub fn set_token(&self, id: u64, digest: &[u8; 32]) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO agent_tokens (agent_id, digest) VALUES (?1, ?2)",
            (id as i64, &digest[..]),
        )?;
        Ok(())
    }

    /// The running agent whose token has this digest.
    pub fn token_agent(&self, digest: &[u8; 32]) -> rusqlite::Result<Option<u64>> {
        self.conn
            .query_row(
                "SELECT t.agent_id FROM agent_tokens t JOIN agents a ON a.id = t.agent_id
                 WHERE t.digest = ?1 AND a.state = 'running'",
                (&digest[..],),
                |r| r.get::<_, i64>(0).map(|id| id as u64),
            )
            .optional()
    }

    /// running -> exited, recording the exit time, and revokes its token.
    /// Errors if the agent is unknown or not running.
    pub fn set_exited(&self, id: u64, code: i32) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let n = self.conn.execute(
            "UPDATE agents SET state = 'exited', exit_code = ?2, exited_unix_ms = ?3
             WHERE id = ?1 AND state = 'running'",
            (id as i64, code, unix_ms_now()),
        )?;
        self.conn
            .execute("DELETE FROM agent_tokens WHERE agent_id = ?1", (id as i64,))?;
        tx.commit()?;
        if n == 1 {
            Ok(())
        } else {
            Err(rusqlite::Error::StatementChangedRows(n))
        }
    }

    /// The exit code of an `exited` agent; `None` if it is unknown or not
    /// exited.
    pub fn exit_code(&self, id: u64) -> rusqlite::Result<Option<i32>> {
        self.conn
            .query_row(
                "SELECT exit_code FROM agents WHERE id = ?1 AND state = 'exited'",
                (id as i64,),
                |r| r.get(0),
            )
            .optional()
            .map(Option::flatten)
    }

    pub fn list_agents(&self) -> rusqlite::Result<Vec<AgentRecord>> {
        self.query_agents(true, |rows| rows.collect())
    }

    /// Like [`Store::list_agents`], but a row that fails to decode is
    /// returned as an error alongside the good rows instead of failing the
    /// whole listing.
    pub fn list_agents_lossy(&self) -> rusqlite::Result<(Vec<AgentRecord>, Vec<rusqlite::Error>)> {
        self.query_agents(true, Self::split_lossy)
    }

    /// [`Store::list_agents_lossy`] without reading labels (every record's
    /// `labels` is empty).
    pub fn list_agents_lossy_unlabeled(
        &self,
    ) -> rusqlite::Result<(Vec<AgentRecord>, Vec<rusqlite::Error>)> {
        self.query_agents(false, Self::split_lossy)
    }

    /// Splits decoded rows into the good ones and the errors.
    fn split_lossy(
        rows: &mut dyn Iterator<Item = rusqlite::Result<AgentRecord>>,
    ) -> rusqlite::Result<(Vec<AgentRecord>, Vec<rusqlite::Error>)> {
        let (mut good, mut bad) = (Vec::new(), Vec::new());
        for row in rows {
            match row {
                Ok(rec) => good.push(rec),
                Err(e) => bad.push(e),
            }
        }
        Ok((good, bad))
    }

    /// The id of every row, including rows that do not decode.
    pub fn agent_ids(&self) -> rusqlite::Result<HashSet<u64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM agents")?;
        let ids = stmt.query_map([], |r| r.get::<_, i64>(0).map(|id| id as u64))?;
        ids.collect()
    }

    /// Every agent's labels, by agent id.
    fn all_labels(&self) -> rusqlite::Result<HashMap<u64, Labels>> {
        let mut stmt = self
            .conn
            .prepare("SELECT agent_id, key, value FROM agent_labels")?;
        let mut all: HashMap<u64, Labels> = HashMap::new();
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get(1)?, r.get(2)?))
        })?;
        for row in rows {
            let (id, key, value) = row?;
            all.entry(id).or_default().insert(key, value);
        }
        Ok(all)
    }

    fn query_agents<T>(
        &self,
        with_labels: bool,
        collect: impl FnOnce(
            &mut dyn Iterator<Item = rusqlite::Result<AgentRecord>>,
        ) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut labels = if with_labels {
            self.all_labels()?
        } else {
            HashMap::new()
        };
        let mut stmt = self.conn.prepare(&format!("{AGENT_COLUMNS} ORDER BY id"))?;
        let mut rows = stmt.query_map([], |r| decode_agent(r, &mut labels))?;
        collect(&mut rows)
    }

    /// running -> lost for every agent left running by a previous daemon.
    pub fn mark_running_as_lost(&self) -> rusqlite::Result<usize> {
        self.mark_running_as_lost_except(&[])
    }

    /// [`Store::mark_running_as_lost`], sparing `keep`: the agents a previous
    /// daemon handed over, still running. One transaction.
    pub fn mark_running_as_lost_except(&self, keep: &[u64]) -> rusqlite::Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let running: Vec<u64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM agents WHERE state = ?1")?;
            let ids = stmt.query_map((AgentState::Running.as_str(),), |r| {
                r.get::<_, i64>(0).map(|id| id as u64)
            })?;
            ids.collect::<rusqlite::Result<_>>()?
        };
        let mut lost = 0;
        for id in running.into_iter().filter(|id| !keep.contains(id)) {
            lost += self.conn.execute(
                "UPDATE agents SET state = ?1 WHERE id = ?2",
                (AgentState::Lost.as_str(), id as i64),
            )?;
            self.conn
                .execute("DELETE FROM agent_tokens WHERE agent_id = ?1", (id as i64,))?;
        }
        tx.commit()?;
        Ok(lost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_agent(n: i64) -> NewAgent {
        NewAgent {
            program: "claude".into(),
            args: vec!["--flag".into(), format!("a b\"{n}")],
            cwd: "/work".into(),
            created_unix_ms: 1_000 + n,
            scrollback_path: format!("/sb/{n}.log"),
            labels: Labels::new(),
            priority: 0,
        }
    }

    #[test]
    fn schema_is_v8() {
        assert_eq!(SCHEMA_VERSION, 8);
    }

    #[test]
    fn tokens_match_running_agents_only() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        let b = s.insert_agent(&new_agent(2)).unwrap();
        s.set_token(a, &[1; 32]).unwrap();
        s.set_token(b, &[2; 32]).unwrap();
        assert_eq!(s.token_agent(&[1; 32]).unwrap(), Some(a));
        assert_eq!(s.token_agent(&[3; 32]).unwrap(), None);
        // A new token replaces the old.
        s.set_token(a, &[4; 32]).unwrap();
        assert_eq!(s.token_agent(&[1; 32]).unwrap(), None);
        assert_eq!(s.token_agent(&[4; 32]).unwrap(), Some(a));
        // Exit revokes it.
        s.set_exited(a, 0).unwrap();
        assert_eq!(s.token_agent(&[4; 32]).unwrap(), None);
        // So does being marked lost, unless spared by a handoff.
        s.mark_running_as_lost_except(&[b]).unwrap();
        assert_eq!(s.token_agent(&[2; 32]).unwrap(), Some(b));
        s.mark_running_as_lost().unwrap();
        assert_eq!(s.token_agent(&[2; 32]).unwrap(), None);
    }

    #[test]
    fn a_tokens_row_goes_with_its_agent() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        s.set_token(a, &[1; 32]).unwrap();
        s.delete_agent(a).unwrap();
        let n: i64 = s
            .conn
            .query_row("SELECT count(*) FROM agent_tokens", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn priority_is_stored_with_the_agent() {
        let (_d, _p, s) = open_tmp();
        let a = s
            .insert_agent(&NewAgent {
                priority: 1,
                ..new_agent(1)
            })
            .unwrap();
        let (b, _) = s
            .insert_agent_with_scrollback(
                &NewAgent {
                    priority: 2,
                    ..new_agent(2)
                },
                |id| format!("/sb/{id}"),
            )
            .unwrap();
        let c = s.insert_agent(&new_agent(3)).unwrap();
        assert_eq!(s.agent(a).unwrap().unwrap().priority, 1);
        assert_eq!(s.agent(b).unwrap().unwrap().priority, 2);
        assert_eq!(s.agent(c).unwrap().unwrap().priority, 0);
        let listed: HashMap<u64, i32> = s
            .list_agents()
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r.priority))
            .collect();
        assert_eq!(listed, HashMap::from([(a, 1), (b, 2), (c, 0)]));
    }

    #[test]
    fn v3_database_migrates_to_v4_with_default_priority() {
        let (_d, p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        drop(s);
        // Back to v3: what a v3 store left behind.
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(
            "DROP TABLE agent_tokens;
             ALTER TABLE agents DROP COLUMN exited_unix_ms;
             ALTER TABLE agents DROP COLUMN priority;
             PRAGMA user_version = 3;",
        )
        .unwrap();
        drop(conn);
        let s = Store::open(&p).unwrap();
        assert_eq!(user_version(&p), SCHEMA_VERSION);
        let rec = s.agent(a).unwrap().unwrap();
        assert_eq!((rec.program.as_str(), rec.priority), ("claude", 0));
    }

    fn open_tmp() -> (tempfile::TempDir, std::path::PathBuf, Store) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let s = Store::open(&path).unwrap();
        (dir, path, s)
    }

    fn user_version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn insert_then_list() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        let b = s.insert_agent(&new_agent(2)).unwrap();
        assert_ne!(a, b);
        let l = s.list_agents().unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].id, a);
        assert_eq!(l[0].program, "claude");
        assert_eq!(l[0].args, vec!["--flag".to_string(), "a b\"1".to_string()]);
        assert_eq!(l[0].cwd, "/work");
        assert_eq!(l[0].state, AgentState::Running);
        assert_eq!(l[0].exit_code, None);
        assert_eq!(l[0].created_unix_ms, 1001);
        assert_eq!(l[0].scrollback_path, "/sb/1.log");
    }

    #[test]
    fn exit_updates_state_and_code() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        assert_eq!(s.list_agents().unwrap()[0].exited_unix_ms, None);
        let before = unix_ms_now();
        s.set_exited(a, 3).unwrap();
        let after = unix_ms_now();
        let l = s.list_agents().unwrap();
        assert_eq!(l[0].state, AgentState::Exited);
        assert_eq!(l[0].exit_code, Some(3));
        let at = l[0].exited_unix_ms.expect("the exit time is recorded");
        assert!(
            (before..=after).contains(&at),
            "{before} <= {at} <= {after}"
        );
        // unknown id and double exit are errors; nothing is overwritten
        assert!(s.set_exited(999, 0).is_err());
        assert!(s.set_exited(a, 9).is_err());
        assert_eq!(s.list_agents().unwrap()[0].exit_code, Some(3));
        assert_eq!(s.list_agents().unwrap()[0].exited_unix_ms, Some(at));
    }

    #[test]
    fn startup_marks_running_agents_lost() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        let b = s.insert_agent(&new_agent(2)).unwrap();
        s.set_exited(b, 0).unwrap();
        assert_eq!(s.mark_running_as_lost().unwrap(), 1);
        let l = s.list_agents().unwrap();
        assert_eq!(
            l.iter().find(|r| r.id == a).unwrap().state,
            AgentState::Lost
        );
        assert_eq!(
            l.iter().find(|r| r.id == b).unwrap().state,
            AgentState::Exited
        );
        assert_eq!(s.mark_running_as_lost().unwrap(), 0);
        // lost agents cannot transition to exited
        assert!(s.set_exited(a, 0).is_err());
    }

    #[test]
    fn handed_over_agents_are_spared_from_lost() {
        let (_d, _p, s) = open_tmp();
        let kept = s.insert_agent(&new_agent(1)).unwrap();
        let other = s.insert_agent(&new_agent(2)).unwrap();
        let done = s.insert_agent(&new_agent(3)).unwrap();
        s.set_exited(done, 0).unwrap();
        assert_eq!(s.mark_running_as_lost_except(&[kept, done, 99]).unwrap(), 1);
        let state = |id| s.agent(id).unwrap().unwrap().state;
        assert_eq!(state(kept), AgentState::Running);
        assert_eq!(state(other), AgentState::Lost);
        assert_eq!(state(done), AgentState::Exited);
    }

    #[test]
    fn reopen_preserves_rows() {
        let (_d, p, s) = open_tmp();
        s.insert_agent(&new_agent(1)).unwrap();
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.list_agents().unwrap().len(), 1);
        let id = s.insert_agent(&new_agent(2)).unwrap();
        assert_eq!(id, 2);
    }

    #[test]
    fn migration_is_idempotent() {
        let (_d, p, s) = open_tmp();
        drop(s);
        let _s = Store::open(&p).unwrap();
        assert_eq!(user_version(&p), SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_is_rejected() {
        let (_d, p, s) = open_tmp();
        drop(s);
        Connection::open(&p)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(Store::open(&p).is_err());
    }

    #[test]
    fn new_db_is_private_and_wal() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, p, s) = open_tmp();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let jm: String = s
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(jm, "wal");
    }

    #[test]
    fn insert_with_scrollback_uses_the_new_id() {
        let (_d, _p, s) = open_tmp();
        let (id, path) = s
            .insert_agent_with_scrollback(&new_agent(1), |id| format!("/sb/{id}.ring"))
            .unwrap();
        assert_eq!(path, format!("/sb/{id}.ring"));
        assert_eq!(s.list_agents().unwrap()[0].scrollback_path, path);
        s.delete_agent(id).unwrap();
        assert!(s.list_agents().unwrap().is_empty());
    }

    #[test]
    fn lossy_list_skips_corrupt_rows() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        let b = s.insert_agent(&new_agent(2)).unwrap();
        s.conn
            .execute(
                "UPDATE agents SET args_json = 'nope' WHERE id = ?1",
                (a as i64,),
            )
            .unwrap();
        assert!(s.list_agents().is_err());
        let (good, bad) = s.list_agents_lossy().unwrap();
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].id, b);
        assert_eq!(bad.len(), 1);
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn labels_are_stored_listed_and_replaced() {
        let (_d, _p, s) = open_tmp();
        let mut rec = new_agent(1);
        rec.labels = labels(&[("project", "fleetd"), ("role", "coder")]);
        let a = s.insert_agent(&rec).unwrap();
        let b = s.insert_agent(&new_agent(2)).unwrap();
        let l = s.list_agents().unwrap();
        assert_eq!(l[0].labels, rec.labels);
        assert!(l[1].labels.is_empty());
        assert_eq!(s.labels(a).unwrap(), Some(rec.labels.clone()));
        assert_eq!(s.labels(b).unwrap(), Some(Labels::new()));
        assert_eq!(s.labels(999).unwrap(), None);

        s.replace_labels(a, &labels(&[("role", "reviewer")]))
            .unwrap();
        assert_eq!(
            s.list_agents().unwrap()[0].labels,
            labels(&[("role", "reviewer")])
        );
        // Deleting an agent deletes its labels.
        s.delete_agent(a).unwrap();
        let n: i64 = s
            .conn
            .query_row("SELECT count(*) FROM agent_labels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn v1_database_migrates_to_v2() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        // The v1 schema exactly as the M1 store created it.
        let conn = Connection::open(&p).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE agents (
                 id INTEGER PRIMARY KEY,
                 program TEXT NOT NULL,
                 args_json TEXT NOT NULL,
                 cwd TEXT NOT NULL,
                 state TEXT NOT NULL CHECK(state IN ('running','exited','lost')),
                 exit_code INTEGER,
                 created_unix_ms INTEGER NOT NULL,
                 scrollback_path TEXT NOT NULL
             );
             PRAGMA user_version = 1;
             COMMIT;
             INSERT INTO agents (program, args_json, cwd, state, exit_code, created_unix_ms, scrollback_path)
             VALUES ('claude', '[\"-p\"]', '/w', 'exited', 3, 42, '/sb/1.ring');",
        )
        .unwrap();
        drop(conn);

        let s = Store::open(&p).unwrap();
        assert_eq!(user_version(&p), SCHEMA_VERSION);
        let l = s.list_agents().unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].program, "claude");
        assert_eq!(l[0].args, vec!["-p".to_string()]);
        assert_eq!(l[0].exit_code, Some(3));
        assert!(l[0].labels.is_empty());
        s.replace_labels(l[0].id, &labels(&[("project", "old")]))
            .unwrap();
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(
            s.list_agents().unwrap()[0].labels,
            labels(&[("project", "old")])
        );
    }

    #[test]
    fn ids_are_not_reused_after_delete() {
        let (_d, _p, s) = open_tmp();
        s.insert_agent(&new_agent(1)).unwrap();
        let b = s.insert_agent(&new_agent(2)).unwrap();
        s.set_exited(b, 0).unwrap();
        assert!(s.forget_agent(b).unwrap());
        assert_eq!(s.insert_agent(&new_agent(3)).unwrap(), 3);
    }

    /// Columns, foreign keys and indexes of every table, for comparing schemas.
    fn schema(path: &Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut out = Vec::new();
        for table in [
            "agents",
            "agent_labels",
            "agent_replacements",
            "agent_tokens",
            "sqlite_sequence",
        ] {
            for pragma in ["table_info", "foreign_key_list", "index_list"] {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM pragma_{pragma}('{table}')"))
                    .unwrap();
                let n = stmt.column_count();
                let rows = stmt
                    .query_map([], |r| {
                        (0..n)
                            .map(|i| {
                                r.get::<_, rusqlite::types::Value>(i)
                                    .map(|v| format!("{v:?}"))
                            })
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap();
                for row in rows {
                    out.push(format!("{table} {pragma} {}", row.unwrap().join(",")));
                }
            }
        }
        let autoinc: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'agents' AND sql LIKE '%AUTOINCREMENT%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        out.push(format!("agents autoincrement {autoinc}"));
        // The full DDL (covers the CHECK constraint), normalised: SQLite
        // quotes the name of a renamed table, keeps the original spacing and
        // puts an added column after the last one's trailing space.
        for table in [
            "agents",
            "agent_labels",
            "agent_replacements",
            "agent_tokens",
        ] {
            let sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            let sql = sql.replace(['"', '\''], "").replace("agents_v3", "agents");
            // ALTER TABLE ADD COLUMN splices `, <column>` in before the `)`.
            let sql = sql.split_whitespace().collect::<Vec<_>>().join(" ");
            let sql = sql.replace(" ,", ",").replace(" )", ")");
            out.push(format!("{table} sql {sql}"));
        }
        out
    }

    #[test]
    fn new_database_is_created_at_v4_like_a_migrated_one() {
        let (_d, fresh, s) = open_tmp();
        assert_eq!(user_version(&fresh), SCHEMA_VERSION);
        let a = s.insert_agent(&new_agent(1)).unwrap();
        s.set_exited(a, 0).unwrap();
        assert!(s.forget_agent(a).unwrap());
        let b = s.insert_agent(&new_agent(2)).unwrap();
        assert_ne!(a, b, "a forgotten id is not reused");
        s.set_exited(b, 0).unwrap();
        let c = s.insert_agent(&new_agent(3)).unwrap();
        s.replace_agent(b, c).unwrap();
        assert_eq!(s.replacements().unwrap(), HashMap::from([(b, c)]));
        drop(s);

        // A v1 DB migrated through v2, v3 and v4 to v5 has the same schema.
        let dir = tempfile::tempdir().unwrap();
        let migrated = dir.path().join("state.db");
        Connection::open(&migrated)
            .unwrap()
            .execute_batch(
                "CREATE TABLE agents (
                     id INTEGER PRIMARY KEY,
                     program TEXT NOT NULL,
                     args_json TEXT NOT NULL,
                     cwd TEXT NOT NULL,
                     state TEXT NOT NULL CHECK(state IN ('running','exited','lost')),
                     exit_code INTEGER,
                     created_unix_ms INTEGER NOT NULL,
                     scrollback_path TEXT NOT NULL
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(Store::open(&migrated).unwrap());
        let fresh_schema = schema(&fresh);
        assert!(
            fresh_schema
                .iter()
                .any(|l| l.starts_with("agent_replacements table_info"))
        );
        assert!(fresh_schema.contains(&"agents autoincrement 1".to_string()));
        assert_eq!(fresh_schema, schema(&migrated));
    }

    #[test]
    fn v2_database_migrates_to_v3_keeping_rows_and_labels() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        let conn = Connection::open(&p).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE agents (
                 id INTEGER PRIMARY KEY,
                 program TEXT NOT NULL,
                 args_json TEXT NOT NULL,
                 cwd TEXT NOT NULL,
                 state TEXT NOT NULL CHECK(state IN ('running','exited','lost')),
                 exit_code INTEGER,
                 created_unix_ms INTEGER NOT NULL,
                 scrollback_path TEXT NOT NULL
             );
             CREATE TABLE agent_labels (
                 agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 PRIMARY KEY (agent_id, key)
             );
             PRAGMA user_version = 2;
             COMMIT;
             INSERT INTO agents (id, program, args_json, cwd, state, exit_code, created_unix_ms, scrollback_path)
             VALUES (7, 'claude', '[]', '/w', 'exited', 0, 42, '/sb/7.ring');
             INSERT INTO agent_labels (agent_id, key, value) VALUES (7, 'project', 'x');",
        )
        .unwrap();
        drop(conn);

        let s = Store::open(&p).unwrap();
        assert_eq!(user_version(&p), SCHEMA_VERSION);
        assert_eq!(
            s.agent(7).unwrap().unwrap().labels,
            labels(&[("project", "x")])
        );
        let n: i64 = s
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let next = s.insert_agent(&new_agent(1)).unwrap();
        assert_eq!(next, 8);
        s.set_exited(next, 0).unwrap();
        assert!(s.forget_agent(next).unwrap());
        assert_eq!(s.insert_agent(&new_agent(2)).unwrap(), 9);
    }

    #[test]
    fn v3_database_migrates_to_v4_keeping_rows_without_an_exit_time() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.db");
        let s = Store::open(&p).unwrap();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        drop(s);
        let conn = Connection::open(&p).unwrap();
        // Back to the v4 shape: no exit time column.
        conn.execute_batch(
            "DROP TABLE agent_tokens;
             ALTER TABLE agents DROP COLUMN exited_unix_ms;
             UPDATE agents SET state = 'exited', exit_code = 2;
             PRAGMA user_version = 4;",
        )
        .unwrap();
        drop(conn);
        let s = Store::open(&p).unwrap();
        assert_eq!(user_version(&p), SCHEMA_VERSION);
        let rec = s.agent(a).unwrap().unwrap();
        assert_eq!(rec.exit_code, Some(2));
        assert_eq!(rec.exited_unix_ms, None);
    }

    #[test]
    fn forget_refuses_running() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        assert!(!s.forget_agent(a).unwrap());
        assert!(s.agent(a).unwrap().is_some());
    }

    #[test]
    fn forget_unknown_is_false_and_lost_is_forgettable() {
        let (_d, _p, s) = open_tmp();
        assert!(!s.forget_agent(999).unwrap());
        let a = s.insert_agent(&new_agent(1)).unwrap();
        assert_eq!(s.mark_running_as_lost().unwrap(), 1);
        assert!(s.forget_agent(a).unwrap());
        assert!(s.agent(a).unwrap().is_none());
    }

    #[test]
    fn replacement_chains_collapse() {
        let (_d, _p, s) = open_tmp();
        for n in 1..=3 {
            s.insert_agent(&new_agent(n)).unwrap();
        }
        s.set_exited(1, 0).unwrap();
        s.replace_agent(1, 2).unwrap();
        s.set_exited(2, 0).unwrap();
        s.replace_agent(2, 3).unwrap();
        assert_eq!(s.replacements().unwrap(), HashMap::from([(1, 3), (2, 3)]));
        assert!(s.agent(1).unwrap().is_none());
        assert!(s.agent(2).unwrap().is_none());
    }

    #[test]
    fn forgetting_a_successor_drops_its_replacements() {
        let (_d, _p, s) = open_tmp();
        s.insert_agent(&new_agent(1)).unwrap();
        s.insert_agent(&new_agent(2)).unwrap();
        s.set_exited(1, 0).unwrap();
        s.replace_agent(1, 2).unwrap();
        s.set_exited(2, 0).unwrap();
        assert!(s.forget_agent(2).unwrap());
        assert!(s.replacements().unwrap().is_empty());
    }

    #[test]
    fn store_is_send() {
        fn is_send<T: Send>() {}
        is_send::<Store>();
    }
}
