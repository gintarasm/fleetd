//! SQLite agent store (decision 0010): one WAL-mode DB per node.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

const SCHEMA_VERSION: i64 = 1;

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
}

pub struct Store {
    conn: Connection,
}

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

impl Store {
    /// Opens (creating with mode 0600 if new) the DB, enables WAL and
    /// migrates to schema v1. Fails on a DB written by a newer schema.
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
        if version < 1 {
            self.conn.execute_batch(
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
                 COMMIT;",
            )?;
        }
        Ok(())
    }

    pub fn insert_agent(&self, rec: &NewAgent) -> rusqlite::Result<u64> {
        let args_json = serde_json::to_string(&rec.args)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.conn.execute(
            "INSERT INTO agents (program, args_json, cwd, state, created_unix_ms, scrollback_path)
             VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
            (
                &rec.program,
                args_json,
                &rec.cwd,
                rec.created_unix_ms,
                &rec.scrollback_path,
            ),
        )?;
        Ok(self.conn.last_insert_rowid() as u64)
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
        let id = self.insert_agent(rec)?;
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

    /// running -> exited. Errors if the agent is unknown or not running.
    pub fn set_exited(&self, id: u64, code: i32) -> rusqlite::Result<()> {
        let n = self.conn.execute(
            "UPDATE agents SET state = 'exited', exit_code = ?2 WHERE id = ?1 AND state = 'running'",
            (id as i64, code),
        )?;
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
        self.query_agents(|rows| rows.collect())
    }

    /// Like [`Store::list_agents`], but a row that fails to decode is
    /// returned as an error alongside the good rows instead of failing the
    /// whole listing.
    pub fn list_agents_lossy(&self) -> rusqlite::Result<(Vec<AgentRecord>, Vec<rusqlite::Error>)> {
        self.query_agents(|rows| {
            let (mut good, mut bad) = (Vec::new(), Vec::new());
            for row in rows {
                match row {
                    Ok(rec) => good.push(rec),
                    Err(e) => bad.push(e),
                }
            }
            Ok((good, bad))
        })
    }

    fn query_agents<T>(
        &self,
        collect: impl FnOnce(
            &mut dyn Iterator<Item = rusqlite::Result<AgentRecord>>,
        ) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut stmt = self.conn.prepare(
            "SELECT id, program, args_json, cwd, state, exit_code, created_unix_ms, scrollback_path
             FROM agents ORDER BY id",
        )?;
        let mut rows = stmt.query_map([], |r| {
            let args_json: String = r.get(2)?;
            let state: String = r.get(4)?;
            Ok(AgentRecord {
                id: r.get::<_, i64>(0)? as u64,
                program: r.get(1)?,
                args: serde_json::from_str(&args_json)
                    .map_err(|e| conversion_err(2, Box::new(e)))?,
                cwd: r.get(3)?,
                state: AgentState::parse(&state)
                    .ok_or_else(|| conversion_err(4, format!("unknown state {state:?}").into()))?,
                exit_code: r.get(5)?,
                created_unix_ms: r.get(6)?,
                scrollback_path: r.get(7)?,
            })
        })?;
        collect(&mut rows)
    }

    /// running -> lost for every agent left running by a previous daemon.
    pub fn mark_running_as_lost(&self) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE agents SET state = ?1 WHERE state = ?2",
            (AgentState::Lost.as_str(), AgentState::Running.as_str()),
        )
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
        }
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
        s.set_exited(a, 3).unwrap();
        let l = s.list_agents().unwrap();
        assert_eq!(l[0].state, AgentState::Exited);
        assert_eq!(l[0].exit_code, Some(3));
        // unknown id and double exit are errors; nothing is overwritten
        assert!(s.set_exited(999, 0).is_err());
        assert!(s.set_exited(a, 9).is_err());
        assert_eq!(s.list_agents().unwrap()[0].exit_code, Some(3));
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
        assert_eq!(user_version(&p), 1);
    }

    #[test]
    fn newer_schema_is_rejected() {
        let (_d, p, s) = open_tmp();
        drop(s);
        Connection::open(&p)
            .unwrap()
            .pragma_update(None, "user_version", 2)
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

    #[test]
    fn store_is_send() {
        fn is_send<T: Send>() {}
        is_send::<Store>();
    }
}
