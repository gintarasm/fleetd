//! Workflow state (`storage:own`, schema v13, ADR 0092): a key-value table
//! per workflow, standing in for the controller store (0019 §4) until there
//! is one.

use rusqlite::OptionalExtension;

use super::{Store, unix_ms_now};

/// `workflow_kv`: one row per (workflow, key).
pub(super) const WORKFLOW_KV_DDL: &str = "CREATE TABLE workflow_kv (
                 workflow TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value BLOB NOT NULL,
                 updated_ms INTEGER NOT NULL,
                 PRIMARY KEY (workflow, key)
             );";

/// A value is at most this many bytes...
pub const KV_MAX_VALUE_BYTES: usize = 64 * 1024;
/// ...a workflow has at most this many keys...
pub const KV_MAX_KEYS: usize = 10_000;
/// ...and a key is 1 to this many bytes.
pub const KV_MAX_KEY_BYTES: usize = 1024;

impl Store {
    /// The value of `workflow`'s `key`.
    pub fn kv_get(&self, workflow: &str, key: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        self.conn
            .query_row(
                "SELECT value FROM workflow_kv WHERE workflow = ?1 AND key = ?2",
                (workflow, key),
                |r| r.get(0),
            )
            .optional()
    }

    /// Sets `workflow`'s `key`. `Ok(Err(reason))` when a limit refuses it
    /// (nothing is written).
    pub fn kv_put(
        &self,
        workflow: &str,
        key: &str,
        value: &[u8],
    ) -> rusqlite::Result<Result<(), String>> {
        if key.is_empty() || key.len() > KV_MAX_KEY_BYTES {
            return Ok(Err(format!(
                "a storage key is 1 to {KV_MAX_KEY_BYTES} bytes"
            )));
        }
        if value.len() > KV_MAX_VALUE_BYTES {
            return Ok(Err(format!(
                "a storage value is at most {KV_MAX_VALUE_BYTES} bytes"
            )));
        }
        let tx = self.conn.unchecked_transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_kv WHERE workflow = ?1 AND key = ?2)",
            (workflow, key),
            |r| r.get(0),
        )?;
        if !exists {
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM workflow_kv WHERE workflow = ?1",
                [workflow],
                |r| r.get(0),
            )?;
            if usize::try_from(count).unwrap_or(usize::MAX) >= KV_MAX_KEYS {
                return Ok(Err(format!(
                    "workflow {workflow:?} has {KV_MAX_KEYS} storage keys (the most it may have)"
                )));
            }
        }
        tx.execute(
            "INSERT INTO workflow_kv (workflow, key, value, updated_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (workflow, key) DO UPDATE SET value = excluded.value, updated_ms = excluded.updated_ms",
            (workflow, key, value, unix_ms_now()),
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Deletes `workflow`'s `key`; whether it was there.
    pub fn kv_delete(&self, workflow: &str, key: &str) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM workflow_kv WHERE workflow = ?1 AND key = ?2",
            (workflow, key),
        )?;
        Ok(n > 0)
    }

    /// `workflow`'s keys starting with `prefix`, sorted.
    pub fn kv_keys(&self, workflow: &str, prefix: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT key FROM workflow_kv WHERE workflow = ?1 AND substr(key, 1, length(?2)) = ?2
             ORDER BY key",
        )?;
        stmt.query_map((workflow, prefix), |r| r.get(0))?.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::open_tmp;
    use super::*;

    #[test]
    fn values_are_per_workflow_and_survive_reopening() {
        let (_d, p, s) = open_tmp();
        assert_eq!(s.kv_get("w", "a").unwrap(), None);
        s.kv_put("w", "a", b"1").unwrap().unwrap();
        s.kv_put("w", "a", b"2").unwrap().unwrap();
        s.kv_put("other", "a", b"x").unwrap().unwrap();
        assert_eq!(s.kv_get("w", "a").unwrap().as_deref(), Some(&b"2"[..]));
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.kv_get("w", "a").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(s.kv_get("other", "a").unwrap().as_deref(), Some(&b"x"[..]));
        assert!(s.kv_delete("w", "a").unwrap());
        assert!(!s.kv_delete("w", "a").unwrap());
        assert_eq!(s.kv_get("w", "a").unwrap(), None);
        assert_eq!(s.kv_get("other", "a").unwrap().as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn keys_list_by_prefix_in_order() {
        let (_d, _p, s) = open_tmp();
        for k in ["wt/b", "wt/a", "x", "wt%", "w"] {
            s.kv_put("w", k, b"").unwrap().unwrap();
        }
        s.kv_put("v", "wt/c", b"").unwrap().unwrap();
        assert_eq!(s.kv_keys("w", "wt/").unwrap(), vec!["wt/a", "wt/b"]);
        assert_eq!(s.kv_keys("w", "").unwrap().len(), 5);
        assert_eq!(s.kv_keys("w", "wt%").unwrap(), vec!["wt%"], "no LIKE");
    }

    #[test]
    fn limits_refuse_big_values_bad_keys_and_too_many_keys() {
        let (_d, _p, s) = open_tmp();
        let big = vec![0u8; KV_MAX_VALUE_BYTES + 1];
        assert!(s.kv_put("w", "a", &big).unwrap().is_err());
        assert!(
            s.kv_put("w", "a", &big[..KV_MAX_VALUE_BYTES])
                .unwrap()
                .is_ok()
        );
        assert!(s.kv_put("w", "", b"").unwrap().is_err());
        assert!(
            s.kv_put("w", &"k".repeat(KV_MAX_KEY_BYTES + 1), b"")
                .unwrap()
                .is_err()
        );
        for i in 1..KV_MAX_KEYS {
            s.kv_put("w", &format!("k{i}"), b"").unwrap().unwrap();
        }
        let e = s.kv_put("w", "one-more", b"").unwrap().unwrap_err();
        assert!(e.contains("10000"), "{e}");
        // Overwriting an existing key is still fine, and other workflows
        // have their own count.
        s.kv_put("w", "k1", b"new").unwrap().unwrap();
        s.kv_put("v", "one-more", b"").unwrap().unwrap();
        assert_eq!(s.kv_get("w", "one-more").unwrap(), None);
    }
}
