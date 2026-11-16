//! The mailbox tables (schema v9, ADR 0076).

use rusqlite::OptionalExtension;

use super::{Store, unix_ms_now};
use crate::mailbox::{AGENT_RETENTION, Mailbox, Message, MessageKind, NewMessage, USER_RETENTION};
use crate::scope;

/// `messages`: one row per message. `recipient` NULL is the user's mailbox;
/// an agent's mailbox goes with its row. `sender` NULL is the user; it is
/// not a reference, so a message outlives its sender. `id` is the cursor:
/// AUTOINCREMENT, so it only grows. `inbox_marks`: how far each mailbox
/// has been read (`mailbox` 0 is the user's).
pub(super) const MAILBOX_DDL: &str = "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 recipient INTEGER REFERENCES agents(id) ON DELETE CASCADE,
                 sender INTEGER,
                 kind TEXT NOT NULL CHECK(kind IN ('message','status','exit','lost','question','answer')),
                 body TEXT NOT NULL,
                 status TEXT,
                 exit_code INTEGER,
                 created_ms INTEGER NOT NULL
             );
             CREATE INDEX messages_by_recipient ON messages(recipient, id);
             CREATE TABLE inbox_marks (
                 mailbox INTEGER PRIMARY KEY,
                 last_read INTEGER NOT NULL
             );";

const MESSAGE_COLUMNS: &str =
    "SELECT id, recipient, sender, kind, body, status, exit_code, created_ms FROM messages";

fn id_param(id: Option<u64>) -> Option<i64> {
    id.map(|id| id as i64)
}

/// The `inbox_marks` key of a mailbox.
fn mark_key(mailbox: Mailbox) -> i64 {
    mailbox.map_or(0, |id| id as i64)
}

fn decode(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let kind: String = r.get(3)?;
    let kind = MessageKind::parse(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            format!("unknown message kind {kind:?}").into(),
        )
    })?;
    Ok(Message {
        id: r.get::<_, i64>(0)? as u64,
        recipient: r.get::<_, Option<i64>>(1)?.map(|id| id as u64),
        sender: r.get::<_, Option<i64>>(2)?.map(|id| id as u64),
        kind,
        body: r.get(4)?,
        status: r.get(5)?,
        exit_code: r.get(6)?,
        created_unix_ms: r.get(7)?,
    })
}

impl Store {
    /// Stores `m` and drops the oldest messages of its mailbox beyond the
    /// retention limit, in one transaction. The new message's id.
    pub fn post_message(&self, m: &NewMessage) -> rusqlite::Result<u64> {
        let tx = self.conn.unchecked_transaction()?;
        let id = self.insert_message(m)?;
        tx.commit()?;
        Ok(id)
    }

    /// [`Store::post_message`] inside the caller's transaction.
    fn insert_message(&self, m: &NewMessage) -> rusqlite::Result<u64> {
        self.conn
            .prepare_cached(
                "INSERT INTO messages (recipient, sender, kind, body, status, exit_code, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute((
                id_param(m.recipient),
                id_param(m.sender),
                m.kind.as_str(),
                &m.body,
                &m.status,
                m.exit_code,
                unix_ms_now(),
            ))?;
        let id = self.conn.last_insert_rowid() as u64;
        let keep = if m.recipient.is_some() {
            AGENT_RETENTION
        } else {
            USER_RETENTION
        };
        self.conn
            .prepare_cached(
                "DELETE FROM messages WHERE recipient IS ?1 AND id <= (
                     SELECT id FROM messages WHERE recipient IS ?1
                     ORDER BY id DESC LIMIT 1 OFFSET ?2)",
            )?
            .execute((id_param(m.recipient), keep as i64))?;
        Ok(id)
    }

    /// The messages of `mailbox` after cursor `after`, oldest first, at
    /// most `limit`.
    pub fn messages_after(
        &self,
        mailbox: Mailbox,
        after: u64,
        limit: usize,
    ) -> rusqlite::Result<Vec<Message>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "{MESSAGE_COLUMNS} WHERE recipient IS ?1 AND id > ?2 ORDER BY id LIMIT ?3"
        ))?;
        let rows = stmt.query_map((id_param(mailbox), after as i64, limit as i64), decode)?;
        rows.collect()
    }

    /// Who wrote to whom: the newest `limit` messages of kind `message`,
    /// `question` or `answer` with an agent as sender and as recipient,
    /// created at or after `since_ms`, oldest first. No bodies (ADR 0089).
    pub fn message_edges(
        &self,
        since_ms: i64,
        limit: usize,
    ) -> rusqlite::Result<Vec<(u64, u64, u64, MessageKind, i64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, sender, recipient, kind, created_ms FROM (
                 SELECT id, sender, recipient, kind, created_ms FROM messages
                 WHERE sender IS NOT NULL AND recipient IS NOT NULL
                   AND kind IN ('message','question','answer') AND created_ms >= ?1
                 ORDER BY id DESC LIMIT ?2) ORDER BY id",
        )?;
        let rows = stmt.query_map((since_ms, limit as i64), |r| {
            let kind: String = r.get(3)?;
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                MessageKind::parse(&kind).unwrap_or(MessageKind::Message),
                r.get::<_, i64>(4)?,
            ))
        })?;
        rows.collect()
    }

    /// How far `mailbox` has been read: the last cursor its owner read.
    pub fn read_mark(&self, mailbox: Mailbox) -> rusqlite::Result<u64> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_read FROM inbox_marks WHERE mailbox = ?1",
                (mark_key(mailbox),),
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map_or(0, |n| n as u64))
    }

    /// Moves `mailbox`'s read mark forward to `to` (never back).
    pub fn mark_read(&self, mailbox: Mailbox, to: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO inbox_marks (mailbox, last_read) VALUES (?1, ?2)
             ON CONFLICT(mailbox) DO UPDATE SET last_read = max(last_read, excluded.last_read)",
            (mark_key(mailbox), to as i64),
        )?;
        Ok(())
    }

    /// The state of the newest `status` message `agent` sent: its latest
    /// reported (or inferred) status, while the message is kept.
    ///
    /// The seam for M3-D, which stores the reported status on the agent:
    /// this then reads that column instead.
    pub fn latest_status(&self, agent: u64) -> rusqlite::Result<Option<String>> {
        Ok(self.latest_status_message(agent)?.and_then(|m| m.status))
    }

    /// The newest `status` message `agent` sent, while it is kept.
    pub fn latest_status_message(&self, agent: u64) -> rusqlite::Result<Option<Message>> {
        self.conn
            .query_row(
                &format!(
                    "{MESSAGE_COLUMNS} WHERE sender = ?1 AND kind = 'status'
                     ORDER BY id DESC LIMIT 1"
                ),
                (agent as i64,),
                decode,
            )
            .optional()
    }

    /// Stores each of `ms` (see [`Store::post_message`]) in one
    /// transaction; their ids.
    pub fn post_messages(&self, ms: &[NewMessage]) -> rusqlite::Result<Vec<u64>> {
        let tx = self.conn.unchecked_transaction()?;
        let ids = ms
            .iter()
            .map(|m| self.insert_message(m))
            .collect::<rusqlite::Result<_>>()?;
        tx.commit()?;
        Ok(ids)
    }

    /// Where `agent`'s automatic reports go: `Some(Some(parent))`, or
    /// `Some(None)` for the user (a root agent, or one whose parent was
    /// removed). `None`: no reports, for a root shell (it is the user, ADR
    /// 0072) and an unknown agent.
    pub fn report_mailbox(&self, agent: u64) -> rusqlite::Result<Option<Mailbox>> {
        let Some(labels) = self.labels(agent)? else {
            return Ok(None);
        };
        if scope::is_root_shell(&labels) {
            return Ok(None);
        }
        let Some(parent) = scope::parent_of(&labels) else {
            return Ok(Some(None));
        };
        let parent = self
            .conn
            .query_row(
                "SELECT new_id FROM agent_replacements WHERE old_id = ?1",
                (parent as i64,),
                |r| r.get::<_, i64>(0).map(|id| id as u64),
            )
            .optional()?
            .unwrap_or(parent);
        let known = self
            .conn
            .query_row(
                "SELECT 1 FROM agents WHERE id = ?1",
                (parent as i64,),
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(Some(known.then_some(parent)))
    }

    /// Posts `m` from `agent` to where its reports go (see
    /// [`Store::report_mailbox`]); the recipient and the message id, or
    /// `None` when the agent's reports go nowhere.
    pub fn post_report(
        &self,
        agent: u64,
        m: impl FnOnce(Mailbox) -> NewMessage,
    ) -> rusqlite::Result<Option<(Mailbox, u64)>> {
        let tx = self.conn.unchecked_transaction()?;
        let posted = self.insert_report(agent, m)?;
        tx.commit()?;
        Ok(posted)
    }

    /// [`Store::post_report`] inside the caller's transaction.
    pub(super) fn insert_report(
        &self,
        agent: u64,
        m: impl FnOnce(Mailbox) -> NewMessage,
    ) -> rusqlite::Result<Option<(Mailbox, u64)>> {
        let Some(to) = self.report_mailbox(agent)? else {
            return Ok(None);
        };
        Ok(Some((to, self.insert_message(&m(to))?)))
    }

    /// The `lost` report of an agent marked lost.
    pub(super) fn report_lost(&self, agent: u64) -> rusqlite::Result<()> {
        self.insert_report(agent, |to| NewMessage {
            recipient: to,
            sender: Some(agent),
            kind: MessageKind::Lost,
            body: "lost: it was running when fleetd stopped".into(),
            status: None,
            exit_code: None,
        })?;
        Ok(())
    }

    /// A restart replaced `old` by `new`: the mailbox and its read mark
    /// move to `new` (inside [`Store::replace_agent`]'s transaction, before
    /// `old`'s row and with it its messages go).
    pub(super) fn move_mailbox(&self, old: u64, new: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET recipient = ?2 WHERE recipient = ?1",
            (old as i64, new as i64),
        )?;
        self.conn.execute(
            "UPDATE OR REPLACE inbox_marks SET mailbox = ?2 WHERE mailbox = ?1",
            (old as i64, new as i64),
        )?;
        Ok(())
    }

    /// Drops a forgotten agent's read mark (its messages cascade).
    pub(super) fn drop_mailbox_mark(&self, agent: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM inbox_marks WHERE mailbox = ?1",
            (agent as i64,),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{new_agent, open_tmp};
    use super::*;
    use crate::store::Labels;

    fn msg(to: Mailbox, from: Option<u64>, body: &str) -> NewMessage {
        NewMessage {
            recipient: to,
            sender: from,
            kind: MessageKind::Message,
            body: body.into(),
            status: None,
            exit_code: None,
        }
    }

    fn bodies(ms: &[Message]) -> Vec<&str> {
        ms.iter().map(|m| m.body.as_str()).collect()
    }

    #[test]
    fn messages_are_read_after_a_cursor_per_mailbox() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        let m1 = s.post_message(&msg(Some(a), None, "one")).unwrap();
        s.post_message(&msg(None, Some(a), "to user")).unwrap();
        let m3 = s.post_message(&msg(Some(a), None, "two")).unwrap();
        assert!(m3 > m1);
        assert_eq!(
            bodies(&s.messages_after(Some(a), 0, 100).unwrap()),
            ["one", "two"]
        );
        assert_eq!(
            bodies(&s.messages_after(Some(a), m1, 100).unwrap()),
            ["two"]
        );
        assert_eq!(bodies(&s.messages_after(Some(a), 0, 1).unwrap()), ["one"]);
        let user = s.messages_after(None, 0, 100).unwrap();
        assert_eq!(bodies(&user), ["to user"]);
        assert_eq!(user[0].sender, Some(a));
        assert_eq!(user[0].recipient, None);
        assert_eq!(user[0].kind, MessageKind::Message);
    }

    #[test]
    fn read_marks_only_move_forward() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        assert_eq!(s.read_mark(Some(a)).unwrap(), 0);
        s.mark_read(Some(a), 5).unwrap();
        s.mark_read(Some(a), 3).unwrap();
        assert_eq!(s.read_mark(Some(a)).unwrap(), 5);
        assert_eq!(s.read_mark(None).unwrap(), 0);
        s.mark_read(None, 9).unwrap();
        assert_eq!(s.read_mark(None).unwrap(), 9);
        assert_eq!(s.read_mark(Some(a)).unwrap(), 5);
    }

    #[test]
    fn an_agents_mailbox_keeps_the_newest_thousand() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        for i in 0..AGENT_RETENTION + 5 {
            s.post_message(&msg(Some(a), None, &i.to_string())).unwrap();
        }
        let kept = s.messages_after(Some(a), 0, usize::MAX >> 1).unwrap();
        assert_eq!(kept.len(), AGENT_RETENTION);
        assert_eq!(kept[0].body, "5");
        assert_eq!(kept.last().unwrap().body, (AGENT_RETENTION + 4).to_string());
    }

    #[test]
    fn the_users_mailbox_keeps_ten_thousand() {
        let (_d, _p, s) = open_tmp();
        let tx = s.conn.unchecked_transaction().unwrap();
        for i in 0..USER_RETENTION + 3 {
            s.insert_message(&msg(None, None, &i.to_string())).unwrap();
        }
        tx.commit().unwrap();
        let kept = s.messages_after(None, 0, usize::MAX >> 1).unwrap();
        assert_eq!(kept.len(), USER_RETENTION);
        assert_eq!(kept[0].body, "3");
    }

    #[test]
    fn a_mailbox_goes_with_its_agent_and_moves_on_restart() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        let b = s.insert_agent(&new_agent(2)).unwrap();
        s.post_message(&msg(Some(a), Some(b), "for a")).unwrap();
        let m = s.post_message(&msg(Some(b), None, "for b")).unwrap();
        s.post_message(&msg(None, Some(a), "from a")).unwrap();
        s.mark_read(Some(b), m).unwrap();

        // A restart moves b's mailbox and mark to its successor.
        s.set_exited(b, 0).unwrap();
        let c = s.insert_agent(&new_agent(3)).unwrap();
        s.replace_agent(b, c).unwrap();
        assert_eq!(
            bodies(&s.messages_after(Some(c), 0, 10).unwrap()),
            ["for b"]
        );
        assert_eq!(s.read_mark(Some(c)).unwrap(), m);

        // `fleet rm` takes a's mailbox; what a sent stays.
        s.set_exited(a, 0).unwrap();
        assert!(s.forget_agent(a).unwrap());
        assert!(s.messages_after(Some(a), 0, 10).unwrap().is_empty());
        assert_eq!(bodies(&s.messages_after(None, 0, 10).unwrap()), ["from a"]);
        s.set_exited(c, 0).unwrap();
        assert!(s.forget_agent(c).unwrap());
        assert_eq!(s.read_mark(Some(c)).unwrap(), 0);
    }

    fn agent_with(s: &Store, n: i64, labels: &[(&str, &str)]) -> u64 {
        let mut rec = new_agent(n);
        rec.labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Labels>();
        s.insert_agent(&rec).unwrap()
    }

    #[test]
    fn reports_go_to_the_parent_else_the_user_and_never_from_a_root_shell() {
        let (_d, _p, s) = open_tmp();
        let root = agent_with(&s, 1, &[]);
        let child = agent_with(&s, 2, &[("fleet.parent", &root.to_string())]);
        let shell = agent_with(&s, 3, &[("kind", "shell")]);
        let child_shell = agent_with(
            &s,
            4,
            &[("kind", "shell"), ("fleet.parent", &root.to_string())],
        );
        assert_eq!(s.report_mailbox(root).unwrap(), Some(None));
        assert_eq!(s.report_mailbox(child).unwrap(), Some(Some(root)));
        assert_eq!(s.report_mailbox(shell).unwrap(), None);
        assert_eq!(s.report_mailbox(child_shell).unwrap(), Some(Some(root)));
        assert_eq!(s.report_mailbox(999).unwrap(), None);

        // A restarted parent is followed to its successor...
        s.set_exited(root, 0).unwrap();
        let root2 = agent_with(&s, 5, &[]);
        s.replace_agent(root, root2).unwrap();
        assert_eq!(s.report_mailbox(child).unwrap(), Some(Some(root2)));
        // ...and a removed one leaves the reports to the user.
        s.set_exited(root2, 0).unwrap();
        assert!(s.forget_agent(root2).unwrap());
        assert_eq!(s.report_mailbox(child).unwrap(), Some(None));
    }

    #[test]
    fn agents_marked_lost_report_it() {
        let (_d, _p, s) = open_tmp();
        let root = agent_with(&s, 1, &[]);
        let child = agent_with(&s, 2, &[("fleet.parent", &root.to_string())]);
        let kept = agent_with(&s, 3, &[("fleet.parent", &root.to_string())]);
        agent_with(&s, 4, &[("kind", "shell")]);
        s.mark_running_as_lost_except(&[kept]).unwrap();
        let to_root = s.messages_after(Some(root), 0, 10).unwrap();
        assert_eq!(to_root.len(), 1);
        assert_eq!(to_root[0].sender, Some(child));
        assert_eq!(to_root[0].kind, MessageKind::Lost);
        let to_user = s.messages_after(None, 0, 10).unwrap();
        assert_eq!(to_user.len(), 1, "the root's loss; none for the shell");
        assert_eq!(to_user[0].sender, Some(root));
    }

    #[test]
    fn the_latest_status_is_the_newest_status_message() {
        let (_d, _p, s) = open_tmp();
        let a = s.insert_agent(&new_agent(1)).unwrap();
        assert_eq!(s.latest_status(a).unwrap(), None);
        for st in ["working", "waiting"] {
            s.post_message(&NewMessage {
                status: Some(st.into()),
                kind: MessageKind::Status,
                ..msg(None, Some(a), "")
            })
            .unwrap();
        }
        s.post_message(&msg(None, Some(a), "a plain message"))
            .unwrap();
        assert_eq!(s.latest_status(a).unwrap().as_deref(), Some("waiting"));
    }
}
